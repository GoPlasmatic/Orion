# Contributing to the Orion book

The system this book follows is [`DOCUMENTATION_STANDARD.md`](./DOCUMENTATION_STANDARD.md).
Read it once; it is the rulebook. This page holds the decisions the standard
tells each set to make for itself, and the deviations this set has recorded.

Section numbers below refer to the standard.

## The navigation axis (§3.1)

**Type-first.** Orion is one product, so the top level is the content model:

| Part | Promise |
|---|---|
| Get started | See it work, then learn how to think about it. |
| Concepts | Understand what each part is and why it behaves as it does. |
| Guides | Complete one task at a time. |
| Operate | Run it in production: deploy, configure, secure, observe, upgrade, recover. |
| Reference | Look up the exact contract. |
| Releases | What changed, what breaks, how to move. |

A promise never changes. A page that fits no promise is in the wrong part.

Persona is never a section. An operator reads Operate guides and the
configuration reference; an AI author reads the *Author with AI* group inside
Guides. Neither gets a tree of its own.

## Page types (§2.1)

Every page declares its type on line 2:

```markdown
<!-- description: One sentence, 110-160 characters, unique across the book. -->
<!-- type: concept -->
<!-- last_verified: 2026-09-14 -->
```

Valid types: `quickstart`, `tutorial`, `concept`, `guide`, `reference`,
`troubleshooting`, `hub`, `release-note`, `migration`, `glossary`.

The type decides the template (§5), the voice register (§6) and where the page
lives (§3). `docs/lint.sh` asserts that the template's sections are present and
in order.

## Freshness stamps (§10.2)

Two stamps, two meanings, and no third:

- `<!-- last_verified: YYYY-MM-DD -->` — a person ran this page and it worked.
  Authored pages carry this.
- `<!-- generated_from: 1.8.0 on YYYY-MM-DD -->` — this page was produced from
  a source at that version. Generated reference carries this.

An index's stamp is the newest of its children and never older than any of
them. `docs/lint.sh` flags a stamp older than the last minor release.

## Titles (§3.6)

The H1, the sidebar label, the browser title, the breadcrumb and the
`llms.txt` entry are one string, in sentence case. Product nouns keep their
capitals: Orion, Kafka, PostgreSQL, Docker, Kubernetes, JSONLogic.

When the sidebar must be shorter, the shortening is mechanical: drop a leading
`How to`, keep the rest. Nothing else is dropped, so a reader arriving from
search can match the two.

No two pages share a title. `docs/lint.sh` check 16 refuses it.

## Callouts (§8)

Four labels, and only four:

```markdown
> **Note.** A fact the reader needs now that does not fit the flow.
> **Tip.** A shortcut or a better way, optional.
> **Warning.** Data loss, security exposure, or an irreversible action.
> **Deprecated.** What replaces it, and when.
```

At most three per page, never two adjacent, never carrying a required step, a
prerequisite, or the only statement of a limit. A warning appears *before* the
step it warns about.

Edition, stability and version gates are not callouts. They are a badge beside
the heading, plus a prose sentence in the page's first paragraph so the fact
survives copying and machine reading (§8.4).

## Code samples (§7)

- One running example across the whole book: an orders service. The
  quickstart's form is workflow `quickstart-orders` on channel `orders` at
  `POST /orders`; the packaged form is `high-value-order`; the database form
  is `postgres-orders` (`record-order` on connector `orders-db`). All three
  ship under `examples/packages/`, so every sample is a tested file.
- Secrets are placeholders: `<YOUR_ADMIN_KEY>` in prose samples,
  `$ORION_ADMIN_KEY` in shell. Never a real key, and never a key in a
  definition file.
- Reserved-for-documentation values only: `example.com`, `user@example.com`,
  `203.0.113.0/24`.
- Commands carry no `$` prompt. Command and output are never in one block; the
  output block is labelled as output.
- One sentence before each block, ending in a colon, saying what it does. It
  never repeats the code.
- A sample that cannot be executed anywhere says so in that sentence.

## Tab order (§7.4)

Where a task has equivalent surfaces, they are tabs on one URL in this order,
site-wide, with identical prose in every tab:

**curl → CLI → Console**

The admin API is the canonical surface; the CLI and the Console wrap it. A
variant that needs different prose is a different page, not a tab.

The markup is plain HTML, so the Markdown twin carries every variant in order:

```html
<div class="tabs">
<section data-tab="curl">

…

</section>
<section data-tab="CLI">

…

</section>
</div>
```

## Diagrams (§9.2)

One notation: the `orion-diagram` fenced block. The same shape means the same
kind of thing on every page — `channel`, `service`, `store`, `external` — and
the same arrow means the same relation. Every diagram has alt text stating what
it shows and is named in the prose, never referred to by position.

No emoji and no character standing in for an icon; `docs/lint.sh` check 14
refuses both. Marks are SVG.

## Recorded deviations

The standard is followed except here. Each entry says what, and why.

| Rule | Deviation | Reason |
|---|---|---|
| §3.3, "a group that outgrows fifteen is two groups" | `Task functions` (27), `orion-server` commands (13), `orion-cli` commands (20), `clippy` rules (18) and `connector` types (7) each stay one group | Each is one machine surface with one catalogue. Splitting the sidebar across it would hide the completeness the hub's routing table exists to show. The hub groups them in prose. |
| §10.4, feedback widget with structured reasons | A "Was this helpful? Send feedback" link in every page footer opens a pre-filled GitHub issue | A widget with reasons needs a tracker endpoint to own, and this project has none. The issue template asks the two questions that matter: what were you trying to do, and what was unclear. |
| §5.7, "a hub has no other content" | `reference/functions/index.md` carries the full summary and retry-safety tables | §5.7.3 permits a routing table on a hub for a section with many pages, and this is that table. The per-function contract is on the function's own page. |
| §7.5, samples included by anchor from a tested repository | Samples are copied, not included | mdBook's `{{#include}}` cannot reach outside `src/`. The repository's own CI executes `examples/` and `tests/e2e/`, and eleven drift tests assert every fact the book restates from the code. |

## Before you open a pull request

```bash
bash docs/build.sh     # mdBook; create-missing = false catches a dangling entry
bash docs/lint.sh      # integrity and structure
vale docs/src          # prose
```

A page that restates a fact from the product — a setting, a route, a default,
an error code, a metric name, a function's input table — must be asserted
against the product by a drift test in `crates/orion-server/tests/integration/`.
The code is authoritative; the build fails when a page disagrees.
