# Orion Docs 2.0 — Restructure & Rewrite Proposal

This proposal restructures and rewrites `docs/` in the spirit of Restate's documentation
([restatedev/docs-restate](https://github.com/restatedev/docs-restate)): simple language,
one clear journey per reader, and strict separation of concepts, how-to, and reference.

It is based on a full read of all 31 current pages, `SUMMARY.md`, `llms.txt`, and the
repo (`README.md`, `examples/`), plus a structural analysis of Restate's navigation and
fourteen of its representative pages. Every change below is grounded in a specific
finding; nothing documents a feature that does not exist.

**The one-line summary:** the facts in Orion's docs are accurate and unusually honest —
the problems are *placement* (content organized by architectural "-ility" instead of
reader task) and *register* (literary, essay-dense prose where Restate uses short
declarative sentences). The fix is a new information architecture plus sentence-level
surgery, not new content invention.

---

## 1. Diagnosis: what is wrong today

### 1.1 Onboarding has no spine

- Getting Started sprawls across 8 sidebar entries pulled from two directories
  (`tutorials/` and `getting-started/`) whose split carries no meaning.
- The real journey — install → first service → first connector → pick your interface —
  exists but is interrupted by Examples (which silently assumes a repo clone no install
  method produces) and terminated by a **19,000-word operator migration guide**
  ("Upgrading to 1.0.0") sharing a section with hello-world.
- The quickstart itself drifts: a brand-new user hits offline testing, regression
  suites, and package promotion before "Next Steps". Its promotion summary is a single
  60-word sentence using "closure" and "drift" unexplained.
- The landing page names the same first step three different ways ("Install Orion and
  ship your first service" / "CLI Setup" / sidebar "Install & First Service") and stacks
  two videos plus an asciinema cast — demo-reel duty that belongs one click deeper.

### 1.2 The "-ility" section is writer-centric

"Production Features" is organized by architectural characteristic. It fails the task
test on nearly every operator job:

| Task | Where it hides today |
|---|---|
| Configure TLS | Six lines inside Security → Network Security |
| Add rate limiting | Scalability |
| Cache responses, idempotency keys | Availability → Performance |
| Back up the database | Maintainability → Operations |
| Cluster / HA deployment | Fourth section of Scalability (plus availability.md, plus environments.md) |
| Set alert rules | Inside metric-table cells in Observability |

The reader must know Orion's internal taxonomy before they can find the answer.

### 1.3 The most-used config surface has no home

Channel `config_json` — `auth`, `rate_limit`, `backpressure`, `deduplication`, `cache`,
`timeout_ms`, `validation_logic`, `origin_allow_list`, response shaping — is the real
API users configure daily. Its keys are scattered across security.md, scalability.md,
availability.md, resilience.md, and api/data.md, each documented wherever its "-ility"
filed it. This is the single largest structural failure in the docs.

### 1.4 Reference is fragmented and mixed-genre

- Three sibling sidebar sections — "API Reference", "Reference", "Configuration" — split
  one reference estate, with a one-page "Tutorials" section wedged between them.
- Reference pages absorb guide content because no how-to layer exists: "Shaped
  Responses" (a feature guide) lives in api/data.md, the promotion walkthrough in
  api/admin.md, the Production Checklist in configuration/reference.md.
- No shared endpoint or field-table template; api/admin.md packs four behaviours into
  single table cells and leaks internal review IDs — (K2), (K7), (R5, R7, K8) — that
  readers cannot resolve.
- `docs/openapi.json` (OpenAPI 3.1, 44 paths) is complete but never surfaced; the
  hand-written pages duplicate its path inventory without its schemas.

### 1.5 The prose register is literary, not instructional

The house style is precise and often charming, but dense: 40–90-word em-dash sentences,
compound-adjective pileups, aphorisms, and ADR-grade rationale essays inline in
operational pages. Real examples:

- "a parameterized insert, then a relation-hydrated read — through a pooled,
  circuit-broken, delete-proof connector" (a *second-hour* user's victory lap)
- "`export` computes the closure (selected channels, their workflows, every connector
  those workflows reference), `lint` and `plan` check it offline / with zero writes,
  `apply` stages and activates in dependency order with a single engine reload, and
  `diff` reports drift afterwards" (one sentence, on the quickstart page)
- "Orion as an upstream that needs far fewer of the gateway's crutches"

Restate's register is the opposite: second person, present tense, under 25 words per
sentence, rule first, rationale behind a link. **Sentence surgery is the single biggest
lever in this project** — the facts underneath are consistently accurate.

Rust internals also leak into user pages: `Arc<RwLock<Arc<Engine>>>`,
`tokio::sync::mpsc`, `channel::guards::apply_guards`, "arena-mode dispatch",
CatchPanicLayer. None serve a builder or operator task.

### 1.6 Duplication has already produced contradictions

Facts with multiple owners have drifted into outright conflicts:

| Contradiction | Where |
|---|---|
| Connector retries: "never re-driven" vs "All connector types (HTTP, DB, Cache, MongoDB, Storage) support the same retry configuration" — including a nonexistent "Storage" type | resilience.md, four paragraphs apart |
| Secret masking: "by allowlist" vs a sensitive-field blocklist | security.md vs extensibility.md |
| "16 built-in functions" vs 18 | reference/workflows.md vs reference/functions.md |
| "OpenAPI 3.0" vs 3.1.0 | api/data.md vs the spec itself |
| "6,000+ requests/sec" vs the measured 5,151–5,655 on the same README | README |
| "46 tools" hard-coded | three separate pages |

Plus verbatim duplicate blocks that haven't drifted *yet*: operation gates JSON
(security.md + extensibility.md), import/export curls (availability.md +
maintainability.md), `[trace_queue]` DLQ config (resilience.md + scalability.md),
`[kafka.dlq]` (resilience.md + extensibility.md), function tables (overview.md +
extensibility.md), health probes (three pages), MCP config JSON (two pages).

### 1.7 Jargon is used before it is taught

"Package", "promotion", "closure", "estate", "receipt", "quarantine", "ingress",
"dialect", "config epoch", "modular monolith" all appear in Getting Started with no
concept page behind them. There are **no concept pages at all**: the clearest existing
explanation of the draft → active → archived lifecycle lives inside the prompt-pack's
pasteable LLM block, where humans are least likely to read it. Three subsystems (trace
queue, cluster mode, entity lifecycle) are each described from three or four pages and
owned by none.

### 1.8 Missing pages readers currently need

Channel config reference · connector type reference · JSONLogic/expressions page ·
error/envelope reference · CLI reference (currently punted to a GitHub README) ·
cluster/HA page · plain Docker deployment guide · production checklist · backup &
restore runbook · troubleshooting page · Kafka on-ramp (a headline capability with no
tutorial) · CI/CD guide built on packages (the existing use-cases.md CI example
contradicts the product's own promotion story, contains a GitHub Actions step that can
never fire, and parses the wrong response shape) · a standing Upgrades home ·
a glossary.

### 1.9 Mechanics hazards

- README.md, llms.txt, and examples/README.md hard-link `goplasmatic.github.io/...html`
  deep URLs; any restructure without redirects breaks them silently.
- First-party content lives off-site: the CLI command list and the examples walkthrough
  are on GitHub, invisible to docs search and `llms-full.txt`.
- architecture/characteristics.md duplicates its own data in Markdown *and* inline JS,
  is stale against the pages it indexes, and loads d3 from a CDN — the docs' only
  external script.

---

## 2. What we adopt from Restate

Restate's docs feel mature because of discipline, not because of Mintlify. What
transfers to mdBook:

1. **Genre discipline (Diátaxis).** Each section owns exactly one content type:
   concept pages explain what/why with minimal code; how-to pages are imperative;
   recipes are self-contained (problem → how Restate helps → full example → "Running
   the example" → related resources); reference pages are normative and unpersuasive
   ("You must not skip minor version upgrades"). The discipline is enforced by page
   *template*, not just placement.
2. **One altitude per page, depth by pointer.** foundations/invocations gives Kafka
   exactly two sentences and links to the reference page and the quickstart guide. The
   same topic appears at four depths (tour → develop → guide → error catalog) with zero
   duplication. In a single mdBook sidebar this matters *more*, because there is no tab
   boundary to hide redundancy.
3. **Concepts precede APIs.** Foundations pages are short (~100–140 lines), SDK-neutral,
   ordered as a reading path, each ending in "Next Steps".
4. **Decisions get comparison tables with "Use for:" selector lines** naming concrete
   workloads, so readers self-select without reading everything.
5. **Optional depth is folded, not paginated** — accordions for failure scenarios and
   architecture detail keep the main path linear.
6. **Doc code is loaded from a tested examples repo** (their `CODE_LOAD::` pragmas →
   our `{{#include}}` from `examples/`, which the e2e suite already runs).
7. **Operational docs follow the lifecycle** (overview → deploy → configure → cluster →
   snapshots → upgrade → monitor), each page opening with the requirement or guarantee
   before the procedure.

What does **not** transfer, and the adaptation:

| Restate | Orion adaptation |
|---|---|
| Tabs (Learn / Docs / Guides / AI) | mdBook part headers in one sidebar, ordered as one reading journey |
| Per-language SDK sections, 4-way code tabs | Collapse entirely: Orion's one "language" is workflow JSON + curl. Pages are a quarter the length |
| MDX Cards / Steps / Accordions / Info | Grouped link lists with one-line benefits · ordered lists with bold step titles · native `<details><summary>` · blockquote callouts |
| 78 KB single-page Tour | A sequenced tutorial spine of small pages |
| Use-case marketing pages, Cloud/BYOC | comparison.md already fills this; omit the rest |

---

## 3. The new table of contents

Part order is deliberate. Get Started sits directly under the two landing pages —
in a tabless sidebar, six concept pages before the install command would bury the one
obvious first click. Concepts follow, then the two Build sections, then Guides
(builder recipes must not sit below the operator estate), then Operate, then one
unified Reference.

```markdown
# Summary

[Introduction](./introduction.md)
[Is Orion Right for You?](./comparison.md)

---

# Get Started

- [Install & Run](./getting-started/install.md)
- [Your First Service](./getting-started/first-service.md)
- [Your First Connector](./getting-started/first-connector.md)
- [Test & Promote a Service](./getting-started/test-and-promote.md)
- [Run the Examples](./getting-started/examples.md)
- [The Console (Orion UI)](./getting-started/console.md)

# Concepts

- [How Orion Works](./concepts/how-orion-works.md)
- [Channels](./concepts/channels.md)
- [Workflows](./concepts/workflows.md)
- [Connectors](./concepts/connectors.md)
- [Packages](./concepts/packages.md)
- [The Entity Lifecycle](./concepts/lifecycle.md)

# Build with AI

- [Build a Service with Claude Code](./ai/claude-code.md)
- [MCP Server Setup](./ai/mcp-setup.md)
- [Prompt Pack (any LLM)](./ai/prompt-pack.md)

# Build

- [Author Workflows](./build/workflows.md)
- [Configure Channels](./build/channels.md)
- [Connect Databases & APIs](./build/connectors.md)
- [Test Workflows Offline](./build/testing.md)
- [Version & Roll Out Changes](./build/versioning.md)

# Guides

- [Worked Examples: Prompt to Service](./guides/worked-examples.md)
- [Common Workflow Patterns](./guides/workflow-patterns.md)
- [Consume from Kafka](./guides/kafka-channels.md)
- [CI/CD with Packages](./guides/ci-cd.md)

# Operate

- [Production Checklist](./operate/production-checklist.md)
- [Deploy with Docker](./operate/docker.md)
- [Deploy on Kubernetes (Helm)](./operate/kubernetes.md)
- [Cluster Mode & High Availability](./operate/cluster.md)
- [Secure an Instance](./operate/security.md)
- [Monitoring & Alerts](./operate/monitoring.md)
- [Traces & Async Processing](./operate/traces.md)
- [Timeouts, Retries & Circuit Breakers](./operate/failure-handling.md)
- [Promote Between Environments](./operate/promotion.md)
- [Back Up & Restore](./operate/backup-restore.md)
- [Audit Logs](./operate/audit-logs.md)
- [Upgrades](./operate/upgrades.md)
  - [Upgrading to 1.0.0](./operate/upgrading-to-1.0.md)
- [Troubleshooting](./operate/troubleshooting.md)

# Reference

- [Admin API](./reference/admin-api.md)
- [Data API](./reference/data-api.md)
- [OpenAPI Specification](./reference/openapi.md)
- [Channel Configuration](./reference/channel-config.md)
- [Workflow Schema](./reference/workflows.md)
- [Expression Language (JSONLogic)](./reference/expressions.md)
- [Task Functions](./reference/functions.md)
- [Connector Types](./reference/connectors.md)
- [Portable Data Dialect](./reference/data-dialect.md)
- [Configuration Reference](./reference/configuration.md)
- [Metrics Reference](./reference/metrics.md)
- [Errors & Response Envelopes](./reference/errors.md)
- [CLI Reference](./reference/cli.md)
- [Design Notes](./reference/design-notes.md)
- [Support & Compatibility](./reference/support.md)
- [Glossary](./reference/glossary.md)
```

56 pages, up from 31. The growth is entirely from splitting multi-genre pages into
single-genre ones and adding the missing lookup pages listed in §1.8 — every new page
in §5 names the existing content that funds it. Deliberately rejected: hub/README
navigation pages (the sidebar already navigates), per-recipe page explosions, and any
page the analysis gives no source material for.

---

## 4. Migration map — every current page

All 31 pages, plus the two meta-files. **Bold actions** change reader-visible URLs and
need redirects (§8).

### Landing

| Current | Action | New home(s) | What changes |
|---|---|---|---|
| introduction.md | Rewrite in place | introduction.md | Becomes a Restate-style landing hub: two-sentence value proposition, bold-lead capability bullets, ONE "First time here?" route naming Install & Run once (today the first step has three names). Cut: the two link-farm sections restating SUMMARY.md, one of two videos (console.md keeps the UI video), the asciinema cast (moves to Your First Service), the primitives table (Concepts owns it). Keep the honest "Why Orion?" paragraph, tightened. Performance claims align with measured benchmark numbers only. |
| comparison.md | Rewrite in place | comparison.md | Structure stays (short answer → one section per neighbor → summary table). Move the data-plane-has-no-auth warning to operate/security.md as owner; keep a one-line pointer. Cut the "vs. writing another microservice" overlap with the intro. Replace literary phrasing ("gateway's crutches") with plain statements. Add "Use for:" selector lines under the table. |

### Getting Started (today: 8 entries across two directories)

| Current | Action | New home(s) | What changes |
|---|---|---|---|
| tutorials/cli-setup.md | **Split 4 ways** | getting-started/install.md + getting-started/first-service.md + build/testing.md + operate/promotion.md | Install/first-run/CLI-install → install.md (absorbs deployability.md's install matrix). Hello-world walkthrough → first-service.md, ending at a verification step and a Next Steps router that includes operate/security.md. "Testing Workflows Offline" + "A regression suite" → build/testing.md. The 60-word promotion sentence is deleted; promotion content → operate/promotion.md. The mid-page Postgres/OTLP configuration digression becomes a link to the config reference. |
| getting-started/first-connector.md | Rewrite in place | getting-started/first-connector.md | Keep the numbered-step structure — it is the best tutorial in the set. Replace the ~70-line inline workflow blob with `{{#include}}` from examples/packages/postgres-orders (e2e-tested single source). Rewrite the "pooled, circuit-broken, delete-proof" victory lap into plain bullets with links. "Switching backends" shrinks to two sentences + link. |
| *(new)* | **New** | getting-started/test-and-promote.md | Closes the beginner arc (install → build → connect → **test → ship**): lint, dry-run with stubs, one small regression case, then export → plan → apply to a second local instance, with observable verification. Sources: cli-setup.md's bottom half rewritten at tutorial altitude; packages.md's "Try it" section. |
| getting-started/examples.md | Rewrite in place | getting-started/examples.md | Add the explicit "clone the repository" step (no install method produces a checkout; the page never says so). Pull the walkthrough content in from examples/README.md so the docs site and llms-full.txt own it. Keep the catalog table. Defer package jargon to concepts/packages.md links. Drop the backwards "New to Orion?" pointer. |
| getting-started/console.md | Keep | getting-started/console.md | Now sole owner of the UI quickstart video. Break the three run setups into a list; move "What you get" above "Run it"; add one honest line on what the console does not cover; delete the contributor-facing recording-pipeline footnote. |
| tutorials/mcp-setup.md | **Move + revise** | ai/mcp-setup.md | Single owner of MCP client setup. State that Claude Code and Claude Desktop configs are identical (one block, one note). Replace the hand-maintained 46-tool table with grouped categories + pointer to the live tool listing; never hard-code the count. Delete the duplicated five-step usage example (claude-code.md owns it). Either add the Cursor/HTTP-transport configs claude-code.md promises, or fix that link text — verify first, do not invent. |
| tutorials/claude-code.md | **Move** | ai/claude-code.md | Flagship of Build with AI. Collapse setup steps 1–3 into one `claude mcp add` + links. Remove "46 tools". Add "In this guide you will:" outcome bullets. |
| getting-started/prompt-pack.md | **Move** | ai/prompt-pack.md | Content keeps — it is a good artifact. Add a provenance note (which release the block matches, how it is regenerated) so the hand-maintained function list has a drift story. Lifecycle rules stay self-contained but link concepts/lifecycle.md as the human-readable twin. |
| getting-started/upgrading.md | **Move + restructure** | operate/upgrading-to-1.0.md (under new operate/upgrades.md) | Leaves Getting Started. Renumber so checklist rows and body sections agree (kill 3b/7b/14b/17b). Break the false-labelled "Smaller behaviour changes" heap (~40 h3s, half the page) into typed groups: renames / security / API shape / behaviour. Keep the excellent what-changed → how-you'll-notice → what-to-do pattern and every detection command. The version-independent "how a rename fails, by surface" policy moves up to the standing operate/upgrades.md. |

### Architecture & Production Features (today: 10 pages)

| Current | Action | New home(s) | What changes |
|---|---|---|---|
| architecture/overview.md | **Split** | concepts/how-orion-works.md + reference/design-notes.md | Primitives, topology, request flow at user altitude, sync/async → the concept page (which also absorbs environments.md's dev-vs-prod table). Cut the 75-line "Before Orion" strawman (comparison.md owns positioning) and duplicate function tables. Internals (`apply_guards`, engine lock diagram labels) → design-notes.md. |
| architecture/characteristics.md | **Drop** | — | Deleted: stale against the pages it indexes, duplicates its own data in Markdown and inline JS, loads d3 from a CDN, and its C/S/B badge taxonomy teaches nothing. Redirect to concepts/how-orion-works.md. Nothing to salvage — the task-named sidebar supersedes every mapping it encodes. |
| features/observability.md | **Split** | operate/monitoring.md + reference/metrics.md | Setup content (logging, Prometheus, OTel, health endpoints, sampling knobs, bind_addr warning) → monitoring, plus a new "What to alert on" section promoted out of metric-table cells (job staleness, status-field-not-HTTP-code, DLQ depth). The metrics table, stripped to one-line descriptions → reference/metrics.md. Health-probe facts get one owner. |
| features/resilience.md | **Split** | operate/failure-handling.md + operate/traces.md + reference/connectors.md | Timeouts (keep the ingress/ceiling table; fold the Kafka max.poll.interval rationale into `<details>`), retries, circuit breakers, shutdown, panic recovery → failure-handling. **Fix the self-contradiction:** delete the stale "all connector types… same retry configuration" sentence and the nonexistent "Storage" type; the normative retry spec lives once in reference/connectors.md. Trace DLQ + `[trace_queue]` → operate/traces.md. Dissolve the "Fault Tolerance" grab-bag heading. |
| features/security.md | **Split** | operate/security.md + reference/channel-config.md + reference/connectors.md | Operator hardening (admin auth, TLS expanded from six lines to a real section, headers, SSRF, connector_encryption_key, origin-allow-list vs CORS as two plain rules) → operate/security.md, which also absorbs comparison.md's data-plane warning. The hidden per-channel auth reference (api_key, HMAC webhooks, uniform-401, quarantine semantics) → channel-config, with the walkthrough in build/channels.md. Secret masking → reference/connectors.md **after resolving the allowlist-vs-blocklist contradiction against code**. |
| features/scalability.md | **Split** | reference/channel-config.md + operate/traces.md + operate/cluster.md + operate/security.md | Rate limiting and backpressure → channel-config (the surprising per-caller bucket-key default becomes a warning callout, not mid-paragraph prose) + a short how-to in build/channels.md. Async trace queue → operate/traces.md. Cluster mode → operate/cluster.md. trusted_proxies → operate/security.md + config reference row. |
| features/deployability.md | **Merge (dissolve)** | getting-started/install.md + operate/docker.md + reference/configuration.md | Thinnest page (~530 words); dissolves entirely. Install methods → install.md. SQLite-needs-a-volume + container guidance → docker.md. Env-var override mechanics and the misspelled-env-var refusal rule → config reference. Reconcile its defaults table against the runtime-capability table (they disagree on Metrics); the config reference becomes the single authority on defaults. |
| features/extensibility.md | **Split** | reference/connectors.md + reference/channel-config.md + guides/kafka-channels.md + reference/configuration.md | Promotion, not burial: the per-type connector field tables, operation gates, env:// resolution, and retry semantics become reference/connectors.md — the de facto best reference content finally titled as one page. Kafka consumer config untangled from the producer connector: server rows → config reference, walkthrough → the Kafka guide. Channel Protocols stub → channel-config. The misleading "Extensibility" framing is retired; the honest extension-surface statement (JSONLogic + connectors + channel_call; no plugin mechanism) lands in concepts/how-orion-works.md. |
| features/availability.md | **Split** | concepts/lifecycle.md + build/versioning.md + reference/channel-config.md + operate/cluster.md + reference/design-notes.md | Seven topics, five homes. Lifecycle concept → concepts/lifecycle.md. Hot reload (benefit first, no `Arc<RwLock<Arc<Engine>>>` lead), canary rollout, version pinning, import/export → build/versioning.md (single owner of the curls now duplicated with maintainability.md). Caching + dedup config → channel-config. Rolling-deploy drain → the deploy pages; epoch/resync → cluster. The superb dedup claim/settle proof and cache-key-header argument → design-notes, leaving two-line guarantees behind. The `CREATE INDEX CONCURRENTLY` migration-authoring guidance leaves user docs entirely (contributor docs). |
| features/maintainability.md | **Split** | operate/backup-restore.md + operate/audit-logs.md + build/testing.md + guides/ci-cd.md | Backup/restore — the most Restate-like writing in the set — anchors the new runbook: per-backend table, "there is no restore endpoint", numbered offline procedure, cluster prohibition. Audit logs → their own page, keeping the rejected-unknown-filter precision and X-Orion-Change-Context. Dry-run testing → build/testing.md. The CI/CD section is superseded by guides/ci-cd.md. The Admin APIs capability table is deleted — the API reference and OpenAPI spec own the inventory. |

### API / Reference / Configuration (today: 7 pages, 3 sections)

| Current | Action | New home(s) | What changes |
|---|---|---|---|
| api/admin.md | **Split** | reference/admin-api.md + operate/promotion.md + reference/errors.md | Authentication first (the page depends on it but files it last), lifecycle summary linking concepts/lifecycle.md, endpoint sections with unpacked table cells (the channel PATCH /status row's four behaviours become sub-bullets). **Strip all internal review IDs** — (K2)…(K14), (R5, R7, K8) — and pre-1.0 asides. The ~140-line export/promotion essay → operate/promotion.md. The error-code table → reference/errors.md (sole owner — no copy stays behind). The on_conflict matrix and secrets round-trip table survive intact. |
| api/data.md | **Move + revise** | reference/data-api.md | Route Resolution stays — it is a model section. Shaped Responses (~60 lines of channel-config teaching) → reference/channel-config.md. Add the missing trace-object field reference (statuses, task_trace_json entries) — currently never enumerated anywhere. Fix "OpenAPI 3.0" → 3.1. Strip history asides. |
| reference/workflows.md | **Split** | reference/workflows.md + reference/expressions.md + build/versioning.md | Narrows to the workflow/task JSON schema, plus a completed data-context section specifying `metadata`'s exact shape (headers, query, path params, channel name) and the reserved `_orion` namespace — today reverse-engineered from one example. The ~110-line JSONLogic catalogue + sharp-edge callouts → expressions.md, sidebar-findable at last. Lifecycle/rollout operations → build/versioning.md. Fix the 16-vs-18 function count; state it in one generated place. |
| reference/functions.md | Rewrite in place | reference/functions.md | The per-function template is the model for the estate — keep it. Repair the broken data_query table (a seven-line paragraph wedged between rows leaves a headerless fragment); restore full field rows to data_write instead of "as in data_query"; normalize the ad-hoc Required-column conventions into one legend; drop the duplicated data-context intro for a link. |
| reference/data-dialect.md | Rewrite in place | reference/data-dialect.md | Register change from design-essay to spec: compress table-cell essays to normative statements; move "Upgrading from 0.3.x" asides to the upgrade guide; delete the trailing Configuration section (config reference owns `[query]`/`[write]`; link it). Keep the parity table pinned to its test file — the page's crown. Add a second worked example near the top. |
| reference/support.md | Revise in place | reference/support.md | Version-support, MSRV, and platform tables keep. The Deprecations essay compresses to the three-sentence policy; "how a rename fails, by surface" moves to operate/upgrades.md — it is standing policy, not 1.0 trivia. |
| configuration/reference.md | **Split** | reference/configuration.md + operate/production-checklist.md + reference/cli.md | The Setting/Default/Env-var/"When to change" tables — the best reference pattern in the docs — move intact, keeping config.toml.example order and test-enforced parity. Kubernetes service-links digression → `<details>`; trusted_proxies compresses to a rule + link. Evicted: "Production Checklist" → its own Operate page; "CLI Commands" → reference/cli.md; the "Built-in Capabilities" matrix is deleted (concepts covers it). |

### Topology & Tutorials (today: 4 pages)

| Current | Action | New home(s) | What changes |
|---|---|---|---|
| topology/environments.md | **Split** | operate/cluster.md + concepts/how-orion-works.md | The instructional core — the commented `[cluster]` TOML and "migrations are a deploy step, not a boot race" — seeds operate/cluster.md. The dev-vs-prod comparison table (the best orientation device in the section) → the concept page. Deleted: both brochure bullet walls ("What makes prod reliable" / "…the dev loop fast") — links replace them. Diagram-only jargon (ArcSwap, config epoch, single-flight) either enters prose with definitions or moves to design-notes. |
| topology/packages.md | **Split** | concepts/packages.md + operate/promotion.md | The strongest page splits by genre, both halves keeping their quality. Concept half (modular monolith, membership, tags, closure defined at first use) → concepts. The five-verb flow, Needs/Writes table, receipts, secrets handling, try-it walkthrough → promotion, which also absorbs api/admin.md's essay. Additions the analysis demands: ORION_ADMIN_TOKEN stated as required against a secured instance, and a failure-modes subsection — what state a mid-apply crash leaves, whether traffic is affected, recovery beyond "the receipt stays staged". |
| topology/kubernetes.md | **Move** | operate/kubernetes.md | Closest page to target quality; edits only. Break the 60+-word six-feature intro sentence into a list; fix the dangling "1.0 operational alerts" reference by linking monitoring's what-to-alert-on; thin the repeated "That's deliberate" tic. Keep the symptom→cause troubleshooting, values table, and exemplary single-node SQLite section. |
| tutorials/use-cases.md | **Split** | guides/worked-examples.md + guides/workflow-patterns.md (+ superseded by guides/ci-cd.md) | The prompt→JSON→curl examples become actually followable: a setup step that creates and activates the channel first (fixing the guaranteed 404), JSON single-sourced from examples/packages/ via `{{#include}}`, the notification example's simulated `email_sent` flagged honestly up front, the `.output` vs `.data.output` inconsistency fixed. The stranded "Common Workflow Patterns" catalog → its own guide. The CI/CD section is **not migrated** — it contradicts the promotion story, contains a GH Actions step that can never fire, and parses the wrong shape; guides/ci-cd.md replaces it. |

### Meta-files

| Current | Action | What changes |
|---|---|---|
| SUMMARY.md | Rewrite | Replaced by §3. Every moved page gets an `[output.html.redirect]` entry (§8). |
| llms.txt | Regenerate | Derived from the new SUMMARY.md in the same commit — its hard .html links would otherwise break silently. The README-vs-llms.txt disagreement about Use Cases resolves automatically once both derive from one source. |

---

## 5. New pages to write

Every page names its funding sources — no page is invented from nothing.

### Concepts (the missing layer; Restate's "Foundations")

Template per page: definition → diagram → bold-lead characteristics → Next Steps.
Target length ~100–140 lines each.

| Page | Purpose | Sources |
|---|---|---|
| concepts/how-orion-works.md | SDK-neutral mental model: three primitives, one request's journey at user altitude, sync vs async, dev-vs-prod topology, and the honest extension-surface statement (JSONLogic + connectors + channel_call; no plugin/WASM mechanism). | overview.md, environments.md table, extensibility.md framing |
| concepts/channels.md | A channel is a service endpoint: protocols, sync vs async with a "Use for:" table, two-sentence tour of the traffic controls (each linking channel-config). | overview.md, extensibility.md stub, data.md route concepts, prompt-pack |
| concepts/workflows.md | Versioned task pipelines: tasks, conditions, the data-context idea, how a channel selects a workflow. Minimal JSON. | workflows.md intro, overview.md |
| concepts/connectors.md | Named reusable connections: type list, secrets by reference, gates and breakers as ideas. | extensibility.md, overview.md |
| concepts/packages.md | The package as the module boundary of the modular monolith; membership, tags, closure and receipt defined in plain words at first use. | packages.md opening, introduction.md closing paragraph |
| concepts/lifecycle.md | draft → active → archived, single-draft-per-ID, active-version immutability, what triggers engine reload — the home the analysis says is missing (currently clearest inside the prompt-pack's machine block). | availability.md, maintainability.md, admin.md, prompt-pack rules |

### Get Started

| Page | Purpose | Sources |
|---|---|---|
| getting-started/install.md | Install server + CLI (all methods), run, verify /health. Nothing else. | cli-setup.md top, deployability.md install matrix |
| getting-started/first-service.md | Hello-world: create workflow + channel, activate, call — curl with CLI equivalents. Hosts the asciinema cast cut from the intro. Ends with verification and a Next Steps router (connector tutorial / console / AI / **secure your instance**). | cli-setup.md "Create Your First Service" |
| getting-started/test-and-promote.md | The arc-closing tutorial (see §4). | cli-setup.md bottom, packages.md try-it |

### Build (the missing how-to layer)

| Page | Purpose | Sources |
|---|---|---|
| build/workflows.md | How a request becomes a context, parse-then-process, task vs workflow conditions, responding, how errors reach the envelope — imperative and short, pointing into the three reference pages. | workflows.md guide-register material, use-cases.md patterns, functions.md framing |
| build/channels.md | Task home for channel config_json (§1.3). Verb-first sections: choose protocol/route, go async, authenticate callers, rate-limit, deduplicate, cache, validate, backpressure, shape responses — each a short how-to; full key tables live in reference/channel-config.md. | security.md, scalability.md, availability.md, data.md, resilience.md fragments |
| build/connectors.md | Create, test, reload; env:// secrets; operation gates; data_query vs db_read choice. | extensibility.md, first-connector.md, admin.md notes |
| build/testing.md | lint, dry-run with stubs, *.case.json suites, test-connectivity — the CI author's page, with the all-or-nothing stubbing rule stated plainly. | cli-setup.md bottom, maintainability.md, examples/workflow-tests |
| build/versioning.md | New versions, ?dry_run activation, canary rollout buckets, ?reload=defer, import/export (single owner of those curls). | availability.md, workflows.md lifecycle ops, admin.md |

### Guides

Fixed recipe skeleton: problem → how Orion helps → full example → numbered "Running
the example" → Related.

| Page | Purpose | Sources |
|---|---|---|
| guides/worked-examples.md | The prompt→JSON→deploy examples, made runnable (§4, use-cases row). | use-cases.md, examples/packages/ |
| guides/workflow-patterns.md | The pattern catalog freed from the bottom of use-cases.md. ⚠ The channel_call composition pattern has **no currently-runnable example** — see §9. | use-cases.md patterns section |
| guides/kafka-channels.md | The missing async on-ramp for a headline capability: enable `[kafka]`, create a Kafka channel, DLQ behaviour, "dedup narrows at-least-once — it does not make Kafka exactly-once". ⚠ Needs a runnable example package first — see §9. | extensibility.md, resilience.md `[kafka.dlq]`, config `[kafka]`, scalability.md semantics |
| guides/ci-cd.md | The promotion pipeline that replaces the contradictory curl-loop example: export from dev → lint + plan as CI gates → apply to staging/prod → diff for drift, in a GitHub Actions workflow that actually fires. | packages.md verbs, maintainability.md CI section, use-cases.md CI (as the anti-pattern) |

### Operate

| Page | Purpose | Sources |
|---|---|---|
| operate/production-checklist.md | Pre-go-live: admin_auth + read-only keys, TLS, metrics bind_addr, docs.enabled, connector_encryption_key, trusted_proxies, SSRF flags — every row one line + link to its owning page. | config reference checklist, items across five pages |
| operate/docker.md | The missing non-Kubernetes guide: single container, SQLite-needs-a-volume, docker-compose.ha.yml walked as the flagship compose topology, rolling upgrade outside k8s. One honest sentence on systemd/VM: we ship containers; to run under systemd, wrap the binary. | deployability.md, environments.md, docker-compose.ha.yml, README |
| operate/cluster.md | Single owner of cluster mode (today split across four pages): requirements (shared Postgres/MySQL + Redis), the `[cluster]` TOML, shared-vs-per-node semantics table, migrations-as-deploy-step, epoch/resync, backup prohibition — plus a short **"when to go cluster"** section using the repo's measured benchmark numbers only (§9). | scalability.md, environments.md, availability.md, maintainability.md, tests/benchmark |
| operate/security.md | Operator hardening in one findable place: admin_auth, front the unauthenticated data plane with a proxy (promoted out of comparison.md), TLS as a full section, headers, SSRF, encryption key, trusted_proxies, origin-allow-list vs CORS as two plain rules. Honest gaps kept ("There is no built-in JWT verification. Front Orion with a proxy that verifies tokens."). | security.md, comparison.md warning, scalability.md |
| operate/monitoring.md | Logging, Prometheus, OTel, health probes; then "What to alert on" freed from table cells. | observability.md |
| operate/traces.md | Single owner of the trace subsystem (today described from three pages, owned by none): storage modes with per-channel override, the queue and its limits, DLQ retry/drain, cleanup, pointer to the profiling payload. | scalability.md, resilience.md, observability.md, config sections |
| operate/failure-handling.md | Timeouts (ingress/ceiling table), retries (contradiction resolved), circuit breakers, shutdown, panic recovery — as tasks. | resilience.md, extensibility.md retry semantics reconciled |
| operate/promotion.md | The package workflow end-to-end: five verbs with the Needs/Writes table, receipts and content-immutability, secrets round-trip, ORION_ADMIN_TOKEN requirement, two-server walkthrough, and the new failure-modes subsection. | packages.md, admin.md essay, examples/packages |
| operate/backup-restore.md | The canonical runbook, promoted from behind the "Maintainability" heading (§4). | maintainability.md |
| operate/audit-logs.md | Who changed what: listing/filtering, rejected-unknown-filter behaviour, X-Orion-Change-Context, retention. | maintainability.md, admin.md |
| operate/upgrades.md | The standing home 1.0 never had: how upgrades work, run preflight first, "how a rename fails, by surface" (moved from support.md), pointer to compatibility policy, per-version guides as children — a slot for 1.1. | upgrading.md framing, support.md policy, preflight behaviour |
| operate/troubleshooting.md | Symptom-indexed, in the upgrade guide's proven symptom→cause→fix voice: channel quarantine lifecycle (⚠ §7), degraded /health, stuck-open breakers, draining the trace DLQ, Kafka ingest degraded, mapping-yields-object/misspelled-operator, empty context without parse_json. | sharp-edge callouts across the estate |

### Reference

| Page | Purpose | Sources |
|---|---|---|
| reference/channel-config.md | **The #1 gap.** Every config_json key in one normative page: route_pattern, protocol/channel_type, response.mode + allowed_headers, auth (api_key/HMAC, quarantine), rate_limit (per-caller default in a warning callout), backpressure, deduplication, cache, timeout_ms, validation_logic, origin_allow_list, per-channel tracing override — with defaults and cross-ingress semantics. | security.md, scalability.md, availability.md, resilience.md, data.md shaped-responses, extensibility.md |
| reference/connectors.md | Per-type field tables (http, db, cache, mongo, es, kafka producer) in the single Field/Type/Required/Default/Description template; env:// resolution; operation gates (moved ahead of the per-type sections that reference them); the normative retry spec (resolved, §7); secret masking behaviour (resolved, §7); circuit-breaker fields. | extensibility.md tables, security.md gates+masking, resilience.md retries, admin.md notes |
| reference/expressions.md | The JSONLogic operator catalogue + sharp edges (misspelled operators become literals; exists takes segments; switch shape) as warning callouts; the datalogic feature boundary stated. | workflows.md catalogue |
| reference/errors.md | The missing envelope reference, sole owner of all error codes: admin envelope + codes registry, data-plane sync envelope ({status, data, errors}), how failed tasks appear in errors[], verbose_errors sanitization, async failure semantics. | admin.md table, data.md, config verbose_errors note, orion-api codes registry |
| reference/metrics.md | Terse table — name, type, labels, one-line meaning. Alerting guidance lives in operate/monitoring.md. | observability.md table |
| reference/cli.md | Both binaries on-site at last: every orion-server subcommand (validate-config, migrate, lint, dry-run, test, test-connectivity, preflight, dump-openapi, package) and the orion-cli command set currently punted to a GitHub README. | config reference CLI section, orion-cli README, cli.rs/package_cli.rs |
| reference/openapi.md | Surfaces the generated contract the hand-written pages shadow: where the 3.1 spec lives (/api/v1/openapi.json, dump-openapi, docs/openapi.json), Swagger UI gating, and the division of labor (semantics in hand-written pages, schemas in the spec). mdBook cannot render OpenAPI; link, don't embed. | docs/openapi.json, cli.rs |
| reference/configuration.md | The config tables relocated intact (§4). Single authority on defaults. | configuration/reference.md, deployability.md env mechanics |
| reference/design-notes.md | The sanctioned home for the ADR-grade essays, so rule statements elsewhere stay one sentence: dedup claim/settle proof, Kafka timeout clamping, cache-key header exclusion, trusted_proxies reasoning, cursor-paging rationale, engine double-Arc reload, guard pipeline. Opens by releasing readers: "You do not need this page to run Orion." | essays extracted from availability.md, resilience.md, scalability.md, data.md, overview.md |
| reference/glossary.md | One-line definitions for the load-bearing vocabulary taught nowhere: channel, workflow, connector, package, closure, receipt, estate, quarantine, config epoch, rollout bucket, dedup, ingress, dialect, modular monolith. | terms as used across the estate |

---

## 6. Writing style guide

The rules below are adapted from Restate's observed practice and target the specific
failure modes quoted in §1.5. They apply to every page.

**Register and sentences**

1. Open every page with a bare orienting sentence — a definition or the task outcome —
   before any heading. No "This page describes…".
2. Hard sentence budget: under 25 words, one idea each. Break every 40+-word em-dash
   chain into rule + separate why. Never stack more than one parenthetical.
   *Rewrite test:* the 60-word promotion sentence must become five sentences or a
   five-row table.
3. Second person, present tense, Orion as active subject: "Orion refuses the misspelled
   key at startup and names the nearest real setting."
4. Marketing register is allowed on exactly two pages: introduction.md and
   comparison.md. No other page may open "Orion provides/handles/supports X, Y, Z out
   of the box."
5. No aphorisms, no compound-adjective pileups ("pooled, circuit-broken, delete-proof").
   Keep the candor — "Deduplication narrows at-least-once; it does not make Kafka
   exactly-once" is the docs' best asset — and state it plainly with the workaround
   attached.

**Structure**

6. One altitude per page. Concepts explain in two sentences and link down; how-to pages
   give the procedure; reference pages give every field. Any topic gets at most two
   sentences outside its owning page.
7. State the rule first, in one short sentence; if the rationale runs past two
   sentences, collapse it into `<details><summary>Why</summary>` or link Design Notes.
8. Headings: verb-first for tasks ("Authenticate callers", "Drain the trace DLQ"), bare
   noun phrases for concepts. H2-dominant, H3 maximum. If a heading needs "and", split
   it. No grab-bags ("Fault Tolerance", "Smaller behaviour changes").
9. Bold-lead bullets wherever lists carry meaning: bold noun phrase, colon, one-line
   payoff. Benefit before mechanism.
10. Tutorials declare outcomes up front ("In this guide, you will:") and end with an
    observable verification step. Recipes follow: problem → how Orion helps → full
    example → numbered "Running the example" → Related.
11. Deep-dive and migration pages open by scoping who needs them: "You do not need this
    page to run Orion on a single node."
12. Every page ends with 2–4 "Next steps"/"Related" links, each with a one-line reason
    to click — never a link farm restating the sidebar.

**Terminology and facts**

13. Introduce jargon by bolding the term and defining it in the same sentence: "The
    **closure**: the selected channels, their workflows, and every connector those
    workflows reference." Glossary terms are never used before a page defines or links
    them.
14. **One owner per fact.** Config keys → reference/configuration.md; channel keys →
    channel-config; connector fields → connectors; metrics → metrics; error codes →
    errors (admin-api links, never copies); endpoints → API pages + OpenAPI. Everyone
    else links. This is what retires the verbatim duplicates and prevents the next
    allowlist-vs-blocklist contradiction.
15. Follow every abstract claim with a concrete instance using real names and values:
    "`requests_per_second: 100` with no `key_logic` admits 100/s **per caller**, not
    100/s total." Surprising defaults go in a warning callout, never mid-paragraph.
16. Comparison tables close with explicit selector lines: "**Use sync channels for:**
    request/response APIs. **Use async for:** long-running jobs polled by trace." Apply
    to sync vs async, SQLite vs Postgres/MySQL, trace modes, db_read vs data_query,
    single-node vs cluster.
17. Reference pages use normative language — "must", "is refused", "defaults to" — with
    zero persuasion, zero history, zero internal review IDs. Pre-1.0 behaviour appears
    only in the upgrade guides. Field tables follow one template: Field / Type /
    Required / Default / Description, with a legend for special Required values.
18. No hand-maintained magic numbers: no tool counts, no function counts stated in more
    than one place, no throughput claims exceeding the measured table. Cite the
    generated source or omit the number.
19. No Rust internals outside reference/design-notes.md: no `Arc<RwLock<Arc<Engine>>>`,
    `tokio::sync::mpsc`, `channel::guards::apply_guards`, CatchPanicLayer, "arena-mode
    dispatch" anywhere else.

**Mechanics of writing**

20. Doc code is single-sourced: JSON examples are pulled from `examples/` via
    `{{#include}}` wherever a runnable twin exists, so the e2e suite keeps doc code
    honest. Hand-typed blocks only for fragments with no runnable counterpart.
21. mdBook-native components only: blockquote callouts for Note/Warning/Tip,
    `<details><summary>` for optional depth, ordered lists with bold step titles for
    procedures, grouped link lists for hubs. No external scripts (the d3 CDN mindmap is
    the cautionary example).

---

## 7. Facts to settle against code before writing

These block their target pages. Each is a small code-reading task, not a doc task.

| Question | Why it blocks | Arbiter |
|---|---|---|
| Connector retry surface: which types retry, which methods, what defaults? resilience.md contradicts itself and names a nonexistent "Storage" type. | reference/connectors.md, operate/failure-handling.md | connector/ + engine/functions/ |
| Secret masking: allowlist (security.md) or sensitive-field blocklist (extensibility.md)? One is wrong. | reference/connectors.md | connector registry masking code |
| Channel quarantine lifecycle: named on three pages, explained nowhere. When does a channel quarantine, what does it serve meanwhile, how does it recover? | operate/troubleshooting.md, channel-config auth section | channel/registry.rs, channel guards |
| Exact `metadata` shape in the data context (headers, query, path params, channel name) and the reserved `_orion` namespace. | reference/workflows.md data-context section | engine context-building code |
| Trace object fields: full status set beyond pending/completed/failed, task_trace_json entry shape. | reference/data-api.md | storage/models, trace repositories |
| Built-in function count (16 vs 18) — and a single includable source for it. | reference/workflows.md, functions.md | engine/handlers.rs (build_custom_functions) + GET /functions |
| Do Cursor / HTTP-transport MCP configs exist? claude-code.md promises them; mcp-setup.md lacks them. | ai/mcp-setup.md | orion-cli MCP implementation |

---

## 8. Mechanics: do not break the web

Published URLs, llms.txt, and the README all point at today's page paths. Ship these in
the same change as the restructure:

1. **Redirects.** Add `[output.html.redirect]` to book.toml with an entry for every
   moved page in §4. Note the limitation: mdBook redirects are page-level only —
   `#fragment` deep links land at page tops silently. The llms.txt regeneration must
   drop fragments, and the link check must cover them.
2. **llms.txt** is regenerated from the new SUMMARY.md in the same commit, and from
   then on derived by a build step, not maintained by hand.
3. **Hard links.** Update README.md's and examples/README.md's
   `goplasmatic.github.io/...html` deep links in the same commit.
4. **CI drift check.** Add a docs lint job: internal link validation (including
   fragments), `{{#include}}` path validation, the function count against
   `GET /functions` output, and llms.txt-vs-SUMMARY parity. This is the enforcement arm
   of style rules 14, 18, and 20.
5. **Single-sourced examples.** Wire `{{#include}}` from `docs/src` to `examples/`
   paths so the e2e suite keeps doc JSON honest (first-connector, worked-examples).
6. The d3 CDN dependency dies with characteristics.md — after this, the book loads no
   external scripts.

---

## 9. Engineering prerequisites (flagged, not smuggled)

Docs must not invent. These pages need repo work first, and the plan says so explicitly:

- **guides/kafka-channels.md** needs a runnable Kafka example package under
  `examples/packages/` before the guide can promise one. Until then the guide builds
  strictly from the existing config surface, or waits.
- **guides/workflow-patterns.md**: no `channel_call` composition example currently
  runs anywhere in the repo. Either add one to `examples/` or flag the pattern as
  config-documented-only. Do not present it as runnable.
- **guides/worked-examples.md**: the notification example's `email_sent` effect is
  simulated. Keep it simulated and say so up front — do not rewrite it around a "real"
  http_call that a self-contained local example cannot honestly hit.
- **operate/cluster.md sizing section**: use only the measured numbers from
  `crates/orion-server/tests/benchmark` (and the README's measured table). No
  extrapolation.
- **README reconciliation**: the README promises "Multi-agent orchestration" and a
  "Sidecar Pattern" that no docs page covers and no design in this proposal documents.
  Explicit task: either produce the evidence and a page, or delete the claims. The
  README's "6,000+ req/s" line must also drop to the measured range.

---

## 10. What this proposal deliberately does not do

- **No hub/README navigation pages.** The sidebar already navigates; a "Guides
  Overview" page would be a link farm (style rule 12).
- **No per-recipe page explosion.** Four guide pages, not seven; split later only if a
  page outgrows itself.
- **No embedded OpenAPI rendering.** mdBook cannot; reference/openapi.md links the spec
  and the runtime Swagger UI instead.
- **No per-language SDK axis.** Orion's one language is workflow JSON + curl; the
  Function Reference plays the role Restate needs four SDK sections for.
- **No invented features, no invented examples** (§9).
- **No prose-quality regression disguised as reorganization.** The migration map binds
  every move to named sentence-level fixes; §6 is the contract for the rewrite pass.

---

## 11. Suggested phasing

Each phase leaves the book shippable.

**Phase 0 — Settle the facts.** Resolve every §7 question against code. Fix the five
live contradictions (§1.6) in place, before anything moves.

**Phase 1 — Reference owners first.** Create the unified Reference part: move the
config tables, split out channel-config, connectors, expressions, errors, metrics,
cli, openapi, design-notes, glossary. Add redirects + llms.txt regeneration + README
link updates + the CI drift check (§8). *Rationale: one-owner-per-fact needs the owners
to exist before every other page can link instead of inline.*

**Phase 2 — The spine.** Split cli-setup into install / first-service; write
test-and-promote; move the AI cluster to `ai/`; write the six Concept pages; rewrite
introduction.md and examples.md; move upgrading under Operate.

**Phase 3 — Operate.** Dissolve the eight -ility pages into the task pages per §4;
write production-checklist, docker, cluster, security, monitoring, traces,
failure-handling, promotion, backup-restore, audit-logs, upgrades, troubleshooting.
Delete characteristics.md.

**Phase 4 — Build + Guides.** Write the five Build how-tos; split use-cases.md into
worked-examples + workflow-patterns; write ci-cd; write kafka-channels once its
example package exists (§9).

**Phase 5 — The style sweep.** Page-by-page pass enforcing §6: sentence surgery,
callout conversion, "Use for:" lines, Next Steps blocks, duplicate-block removal.
Verify zero occurrences of the §1.6 magic numbers outside their generated sources.

---

*Produced from a full analysis of the current docs (31 pages), the Restate docs
(navigation + 14 representative pages), two independently drafted candidate
architectures, and an adversarial cross-review of both. The structure above is the
journey-first design, reordered per the review (Get Started above Concepts; Guides
above Operate) and grafted with the strongest elements of the Restate-shape design
(the arc-closing tutorial, the design-notes split, the mechanics rigor, and the
enforceable style rules).*
