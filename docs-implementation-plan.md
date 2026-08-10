# Docs 2.0 — Implementation Plan

Execution plan for [`docs-proposal.md`](./docs-proposal.md). The proposal says *what* and
*why*; this plan says *in which order, in which PR, with which exact file operations and
checks*. Every §7 "settle against code" question from the proposal has now been resolved
by reading the code — the answers are in §2 and change a few details of the proposal
(§1.2 lists the amendments).

Phases map to PRs. Each phase leaves the book building and deployable
(`mdbook build docs` with `create-missing = false` is the gate on every PR).

> **Status (2026-08-10):** Phase 0 ✅ complete (committed on `v1.0.0`).
> Phase 1 in progress. Decisions D1–D5: proceeding with the stated defaults.

---

## 1. Readiness assessment

### 1.1 Infrastructure verified

| Fact | Consequence |
|---|---|
| mdBook **0.5.2** pinned in both `ci.yml` (book job) and `docs.yml` (deploy) | `[output.html.redirect]` is supported **including `#fragment` redirects** (JS-based, works for deleted pages). Native admonitions (`> [!NOTE]`/`[!WARNING]`/`[!TIP]`) are available and enabled by default. Unknown config keys are a hard error, so a typo in the redirect table fails the build loudly — good. |
| `docs.yml` already generates `llms-full.txt` from SUMMARY.md order at deploy time | The llms.txt-from-SUMMARY generator reuses this exact pattern; new SUMMARY paths (`./ai/...`, `./operate/...`) match its existing grep. |
| `ci.yml` has a book-build job on PRs | The per-PR gate already exists; the new docs-lint checks bolt onto it. |
| Hard inbound links: **61 total** — README.md (24), docs/src/llms.txt (30), examples/README.md (7), including 3 `#fragment` deep links (`overview.html#three-primitives`, `overview.html#deployment-topology`, `data.html#route-resolution`) | Fully enumerable; §4 redirect map + link rewrite covers every one. llms.txt is regenerated anyway. |
| All example packages the proposal depends on exist: `postgres-orders`, `order-classification`, `high-value-order`, `iot-sensor-alert`, `notification-routing`, `webhook-transform`, plus `workflow-tests/*.case.json` and `use-cases/` | `{{#include}}` single-sourcing is implementable now for first-connector and worked-examples. |
| README carries the measured benchmark table (5,655 / 5,167 / 5,151 req/s, 58K health baseline, v1.0.0) | The cluster-sizing section has its numbers; the "6,000+" claims (README lines 26, 39) exceed them and must drop. |

### 1.2 Amendments to the proposal (superseded details)

Findings from code reading and tooling checks that correct `docs-proposal.md`:

1. **§8.1 fragment caveat is obsolete.** mdBook 0.5 supports fragment redirects
   (`"/old.html#frag" = "new.html#frag"`). We can and should map the three known
   external fragment links instead of dropping fragments from llms.txt.
2. **§6 rule 21: use native admonitions**, not bare blockquote callouts:
   `> [!NOTE]`, `> [!WARNING]`, `> [!TIP]`. Keep `<details><summary>` for folded depth.
3. **§8.4 function-count check must not use `GET /functions`.** That endpoint returns
   only the **10** Orion handlers that have input schemas. The documented count of
   **18** (= 8 dataflow-rs self-contained functions + 10 Orion handlers, with
   `validate` as an alias of `validation`) matches `known_functions()` in
   `engine/handlers.rs` + `CUSTOM_HANDLER_FUNCTIONS` in `engine/loader.rs`. The check:
   the string "18" as a function count may appear **only** in `reference/functions.md`;
   every other page links without a number. (`reference/workflows.md`'s "16" is stale.)
4. **Quarantine is not an auth state.** It is a *channel-load* failure state (see §2.3).
   The proposal filed "quarantine semantics" under the channel-config auth section;
   correct homes: **operate/troubleshooting.md** owns the lifecycle (symptom-first),
   **concepts/lifecycle.md** gets two sentences (activation pre-checks vs load-time
   quarantine), and channel-config's auth section only cross-references it (an
   uncompilable auth block — e.g. unset `env://` secret — is *one* trigger).
5. **Circuit breakers are global, not per-connector.** Config lives at
   `[engine.circuit_breaker]` (`enabled` default **false**, `failure_threshold` 5,
   `recovery_timeout_secs` 30, `max_breakers` 10000; instances per
   `channel:connector`, scope per-node). So: fields belong in
   **reference/configuration.md**; **reference/connectors.md** documents behavior
   (what trips, per-instance isolation, LRU eviction) and links the config table.
6. **Trace statuses are four**: `pending → running → completed | failed`. Detail
   responses use `error`; list rows use `error_message`. `task_trace_json` is
   dataflow-rs `ExecutionTrace`: `{steps: [...], truncated?}` with
   `result: "executed"|"skipped"` and `duration_us` (microseconds), gated by channel
   `config.tracing.task_details` (default false). reference/data-api.md documents
   exactly this.
7. **MCP**: Streamable HTTP transport is implemented (`orion-cli mcp serve --http`,
   endpoint `/mcp`, default bind `0.0.0.0:8081`). A complete Cursor config already
   exists in `crates/orion-cli/README.md:347-364` — port it verbatim. A client-side
   HTTP config exists nowhere and must be newly authored (and `server.json` advertises
   stdio only) — see decision D4.

---

## 2. Settled facts (proposal §7 — answered)

The seven blockers are resolved. Condensed rulings for the writers; agent citations
in the file history of this plan.

### 2.1 Connector retries — HTTP-only, enforced

- Only `http` connectors have a `retry` block; a `retry` key on any other type is
  **rejected with 400** at the admin door. Allowed keys: `max_retries` (default 3,
  max 16), `retry_delay_ms` (default 1000). Backoff: exponential ×2, capped 60 s;
  whole-loop deadline `timeout_ms × (max_retries + 1)`.
- Retried methods: **idempotent only** (GET, PUT, DELETE). POST/PATCH retry only with
  `retry_non_idempotent: true` (default false).
- Retryable errors: HTTP ≥500, 429, 408, status 0, timeouts, I/O.
- **"Storage" is not a connector type.** Removed in 1.0; stored rows surface as a
  `removed_type` load issue. Valid types: `http`, `kafka`, `db`, `cache`, `es`.
- Docs ruling: resilience.md's "All connector types … support the same retry
  configuration" sentence is wrong on three counts — delete it (task T0.1).

### 2.2 Secret masking — allowlist (security.md is right)

- Connector API responses mask by **allowlist** (`READABLE_KEYS` in
  `connector/masking.rs`); any key not on the list is masked — unanticipated secrets
  fail closed. `env://`/`vault://` references pass through unmasked; URL-shaped values
  get in-band userinfo/query redaction; `connection_string` and all header values are
  always masked.
- The old substring **denylist still exists but only serves `validate-config`**
  (CLI dump). extensibility.md describes the denylist as the API policy — wrong (T0.1).
- Channel configs use a third, exact-field rule: `auth.keys[*]` and `auth.secret`.
- Code hygiene: the doc-comment on `mask_secrets()` (`masking.rs:531`) still claims it
  is the connectors-endpoint policy — stale, fix in T0.2.

### 2.3 Channel quarantine — load-failure state, not auth

- **Trigger:** a channel fails to build during engine reload — missing/inactive
  workflow, unusable task (unknown function), unparseable `config_json`,
  uncompilable `rate_limit.key_logic` / `validation_logic` / `auth` block (e.g. unset
  `env://` secret), rollout percentages ≠ 100, or (cluster mode only) unreachable
  dedup/cache backend. No counts, no windows: one failed build = quarantined for that
  snapshot.
- **Behavior:** removed from name lookup *and* route table — REST-routed calls 404,
  name-addressed calls get **503** `SERVICE_UNAVAILABLE` ("failed to load and is not
  being served: {reason}"). Kafka and async-queue deliveries go to DLQ with reason
  `channel_quarantined` (replayable). `channel_call` into it fails the calling task.
- **Recovery:** only the next successful reload (admin mutation, `POST
  /admin/engine/reload`, or cluster resync). No TTL, no per-channel reset.
- **Visibility:** `GET /health` → `components.channels: "degraded"` + per-channel
  `{channel, reason}` array (detail gated behind admin auth when enabled; HTTP status
  stays 200). Boot/reload error logs. **No dedicated metric**;
  `orion_errors_total{reason="channel_quarantined"}` fires on Kafka/async paths only —
  the sync path emits nothing (candidate improvement, out of docs scope).
- Activation pre-checks (`?dry_run=true`, preflight) exist to prevent most quarantines
  — that contrast is the story troubleshooting.md tells.

### 2.4 Data context — exact shape for reference/workflows.md

- Context top level: exactly `data`, `metadata`, `temp_data`. **`payload` is a sibling
  field, not in the JSONLogic context** — `{"var": "payload.x"}` resolves to nothing;
  workflows must start with `parse_json {source: "payload"}`. `data` and `temp_data`
  start `{}`.
- `metadata`, HTTP path: caller-supplied metadata object, then server-stamped:
  `channel` (always overrides), `http_method`, `params` (path params, only when
  non-empty), `query` (only when non-empty), `headers` (all, lowercased; values of
  `authorization`, `cookie`, `proxy-authorization`, `x-api-key` masked). No client_ip,
  no path, no trace id.
- `metadata`, Kafka path: `channel`, `kafka_topic`, `kafka_partition`, `kafka_offset`,
  `kafka_key` (UTF-8 only). **No headers.**
- `metadata`, channel_call path: parent metadata inherited, `channel` overwritten,
  plus `_orion_call_depth` and `_orion_call_chain`.
- `_orion` namespace: `data._orion.response` (`status`/`headers`/`body` — shaped
  channels only, drained before output); `_orion.profile` (top-level envelope key,
  profiling only); the two `metadata._orion_call_*` keys. Nothing else.
- Two *separate* mini-contexts (document distinctly in channel-config):
  `rate_limit.key_logic` sees `{client_ip, channel, headers⊂8}`;
  `validation_logic` sees `{data: <raw payload>, metadata}` — pre-`parse_json`.
- Code hygiene: `queue/mod.rs:128-132` doc-comment references `metadata._orion_profile`
  which the code doesn't write — stale, fix in T0.2.

### 2.5 Function count — 18 is right, 16 is stale

functions.md's "18 functions (plus `validate`, an alias)" matches the code (8
dataflow-rs self-contained + 10 Orion handlers; `enrich` deliberately rejected).
workflows.md's "16 built-in functions" is stale → fix in T0.1, and thereafter no page
except functions.md states a count.

### 2.6 Trace objects — see amendment §1.2.6

Also for reference/data-api.md: list rows deliberately withhold `input_json`,
`result_json`, `task_trace_json`, `access_token_hash` (test-enforced); detail adds
`message` (completed only, `context.metadata` stripped), `error` (failed only),
timestamps, `duration_ms`, `task_trace_json`. Access: admin key or
`x-trace-token`/`?token=`.

### 2.7 MCP transports — see amendment §1.2.7

---

## 3. Decisions needed (before or during the phases)

| # | Decision | Blocks | Default if undecided |
|---|---|---|---|
| D1 | README promises "Multi-agent orchestration" and "Sidecar Pattern" — document (needs evidence) or delete the claims? | T0.1 | Delete the claims (docs must not invent) |
| D2 | Build a runnable Kafka example package under `examples/packages/`? | guides/kafka-channels.md (Phase 4) | Write the guide from the existing config surface only, no "runnable example" promise |
| D3 | Add a runnable `channel_call` composition example to `examples/`? | guides/workflow-patterns.md (Phase 4) | Mark the pattern config-documented-only |
| D4 | Author a client-side HTTP-transport MCP config (new content, no repo precedent; `server.json` advertises stdio only)? | ai/mcp-setup.md (Phase 2) | Document server-side `--http` mode + point clients at `http://host:8081/mcp`; verify against one real client before publishing a JSON block |
| D5 | Redirect targets for multi-destination splits (one old URL → one new URL). Choices in §4 pick the dominant reader intent. | Phases 1–3 | Use §4 as written |

---

## 4. Redirect map (book.toml `[output.html.redirect]`)

Added incrementally — each phase ships the entries for the pages it moves. Values are
the §4 dominant-intent targets; splits get fragment redirects where an external link is
known to point at a specific section.

```toml
[output.html.redirect]
# Phase 1 — reference estate
"/api/admin.html" = "reference/admin-api.html"
"/api/data.html" = "reference/data-api.html"
"/api/data.html#route-resolution" = "reference/data-api.html#route-resolution"
"/configuration/reference.html" = "reference/configuration.html"

# Phase 2 — spine, concepts, AI
"/tutorials/cli-setup.html" = "getting-started/install.html"
"/tutorials/mcp-setup.html" = "ai/mcp-setup.html"
"/tutorials/claude-code.html" = "ai/claude-code.html"
"/getting-started/prompt-pack.html" = "ai/prompt-pack.html"
"/getting-started/upgrading.html" = "operate/upgrading-to-1.0.html"

# Phase 3 — operate + topology + architecture
"/architecture/overview.html" = "concepts/how-orion-works.html"
"/architecture/overview.html#three-primitives" = "concepts/how-orion-works.html#three-primitives"
"/architecture/overview.html#deployment-topology" = "concepts/how-orion-works.html#deployment-topology"
"/architecture/characteristics.html" = "concepts/how-orion-works.html"
"/features/observability.html" = "operate/monitoring.html"
"/features/resilience.html" = "operate/failure-handling.html"
"/features/security.html" = "operate/security.html"
"/features/scalability.html" = "reference/channel-config.html"
"/features/deployability.html" = "operate/docker.html"
"/features/extensibility.html" = "reference/connectors.html"
"/features/availability.html" = "build/versioning.html"
"/features/maintainability.html" = "operate/backup-restore.html"
"/topology/environments.html" = "operate/cluster.html"
"/topology/packages.html" = "concepts/packages.html"
"/topology/kubernetes.html" = "operate/kubernetes.html"

# Phase 4 — guides
"/tutorials/use-cases.html" = "guides/worked-examples.html"
```

Unchanged URLs (no redirect): `introduction`, `comparison`,
`getting-started/{examples,console,first-connector}`,
`reference/{workflows,functions,data-dialect,support}`.

Before each phase merges, re-run the inbound-link sweep and add any newly discovered
fragment link to the table:

```bash
grep -oE "goplasmatic\.github\.io/Orion/[A-Za-z0-9_./#-]+" \
  README.md examples/README.md docs/src/llms.txt | sort -u
```

---

## 5. Work plan by phase

### Phase 0 — Fix live errors in place (PR 1, small)

No structure changes. Everything here is wrong *today* and stays wrong-or-right
independently of the restructure.

- [x] **T0.1 Doc corrections** (all rulings from §2): ✅ done. Deviations from the
  list below, found while executing: "6,000+" also appeared twice in
  `introduction.md` (fixed, same phrasing as README); "46 tools" had **four**
  editable occurrences (README, prompt-pack, claude-code, mcp-setup — all fixed)
  plus `casts/mcp.cast`, which is a recorded terminal session — left as-is, and the
  T6.1 guard grep excludes `docs/src/casts/`; resilience.md's "skips non-retryable
  ones (4xx client errors)" was also corrected (429/408 *are* retried, per §2.1);
  D1 default applied — multi-agent bullet deleted, Sidecar subgraph + "sidecars"
  mention removed from the README topology diagram (overview.md's sidecar diagram
  left for its Phase 2/3 dissolution).
  - `features/resilience.md`: delete the "All connector types … same retry
    configuration" sentence and the "Storage" mention; align with §2.1.
  - `features/extensibility.md`: replace the denylist masking sentence with the
    allowlist rule (§2.2).
  - `reference/workflows.md`: "16 built-in functions" → link to functions.md without
    a number.
  - `api/data.md`: "OpenAPI 3.0" → 3.1; add `running` to the trace status list.
  - `README.md` lines 26 & 39: "6,000+ requests/sec" → measured phrasing
    ("5,100–5,700 workflow req/s measured on v1.0.0", keep the 58K health baseline).
  - `tutorials/mcp-setup.md`, `tutorials/claude-code.md`, + third occurrence: drop the
    hard-coded "46 tools" count (grep to find all three).
  - Per D1: reconcile or delete README's "Multi-agent orchestration" / "Sidecar
    Pattern" claims.
- [x] **T0.2 Stale code comments**: ✅ done (both fixed; `mask_secrets` doc now
  points at `mask_connector_secrets` as the API policy, `QueueMessage` doc now
  names top-level `_orion.profile`).
  - `crates/orion-server/src/connector/masking.rs:531` — `mask_secrets` doc-comment
    claims it is the connectors-endpoint policy; it is validate-config-only.
  - `crates/orion-server/src/queue/mod.rs:128-132` — references
    `metadata._orion_profile`; code writes top-level `_orion.profile`.
- [x] **T0.3 Acceptance:** ✅ `mdbook build docs` clean; masking unit tests
  53/53 pass; both grep guards return zero (46-tools guard scoped with
  `':!docs/src/casts'`).

### Phase 1 — Unified Reference estate (PR 2, the big structural one)

Creates the owners that every later page links to instead of inlining.

- [x] **T1.1 Moves:** ✅ done (PR 2a). Deviations/notes: admin-api gained a
  dedicated "Status changes" section (dry_run + reload=defer moved out of
  Export & Promotion; both PATCH table cells now link it); the promotion essay
  is parked behind a `TODO(docs2)` comment as planned; data-api's `/health`
  row was corrected against §2.3 (200-degraded vs 503 was conflated) and the
  §2.6 trace-object reference was added; "Shaped Responses" remains in
  data-api until channel-config.md lands in T1.2. Original plan text:
  `api/admin.md` → `reference/admin-api.md`;
  `api/data.md` → `reference/data-api.md`;
  `configuration/reference.md` → `reference/configuration.md`.
  Apply the §4-of-the-proposal surgery on each (auth first; unpack table cells; strip
  review IDs `(K…)`/`(R…)`; history → upgrade guide; trace-object reference added to
  data-api per §2.6; "Shaped Responses" content moves to channel-config in T1.2).
  Exception: admin-api's ~140-line promotion essay **stays in place, marked with a
  comment**, until operate/promotion.md exists (Phase 3) — no duplicate ownership
  window.
- [ ] **T1.2 New reference pages** (extraction sources per proposal §5):
  `channel-config.md` (assembled from security/scalability/availability/resilience/
  data.md fragments — the #1 gap; include the two mini-contexts from §2.4),
  `connectors.md` (per-type field tables; retry spec per §2.1; masking per §2.2;
  breaker *behavior* per §1.2.5),
  `expressions.md` (operator catalogue out of workflows.md),
  `errors.md` (sole owner of both envelopes + codes; admin-api links, no copy),
  `metrics.md` (terse table from observability.md),
  `cli.md` (both binaries; ends the GitHub-README punt),
  `openapi.md` (link the spec, Swagger UI gating),
  `design-notes.md` (ADR essays; opens by releasing readers),
  `glossary.md`.
  `reference/workflows.md` narrows (schema + data-context per §2.4);
  `reference/functions.md` and `data-dialect.md` revised in place;
  `support.md` trimmed (rename-policy text parked until operate/upgrades.md exists).
- [x] **T1.3 SUMMARY.md:** ✅ done — one Reference part (interim: the one-page
  Tutorials section moved above it; dissolves in Phase 4).
- [x] **T1.4 Mechanics:** ✅ done (PR 2a) — Phase-1 redirect block in book.toml
  (values made relative, the site serves under the `/Orion/` subpath);
  README/examples/llms.txt links rewritten; `docs/lint.sh` + CI step added.
  Extra fixes while sweeping: llms.txt's "46 MCP tools" (a spelling the Phase-0
  grep missed — guard now covers both spellings), extensibility.md's wrong
  idempotent-method list (claimed HEAD/OPTIONS/TRACE; code allows GET/PUT/DELETE),
  upgrading.md's literal `docs/src/configuration/reference.md` path.
- [ ] **T1.5 Acceptance:** build clean; lychee link-check over `docs/book` green;
  llms.txt parity check green; redirect files exist in `docs/book/api/` etc.
  (`test -f docs/book/api/admin.html` after build and grep it for the redirect target).

### Phase 2 — Spine, Concepts, Build-with-AI (PR 3)

- [ ] **T2.1 Get Started spine:** split `tutorials/cli-setup.md` →
  `getting-started/install.md` + `getting-started/first-service.md` (asciinema cast
  moves here from the intro; Next Steps router includes operate/security.md — as a
  forward-named link that lands in Phase 3, so point it at the old
  `features/security.md` until then and swap in Phase 3);
  write `getting-started/test-and-promote.md` (the arc-closer);
  rewrite `examples.md` (clone step; pull in examples/README.md walkthrough);
  rewrite `first-connector.md` with `{{#include}}` from postgres-orders;
  console.md tweaks.
- [ ] **T2.2 Concepts:** the six pages per proposal §5 (how-orion-works absorbs
  overview.md's user-level half **now**; overview.md becomes a thin pointer page until
  Phase 3 deletes it — or move it in this PR and ship the architecture redirects
  early; prefer the latter if the PR stays reviewable). Quarantine gets its two
  concept-level sentences per §1.2.4.
- [ ] **T2.3 Build with AI:** move the three pages to `ai/`; single-owner MCP setup;
  port the Cursor config from `crates/orion-cli/README.md`; HTTP transport per D4;
  de-duplicate the five-step sequence; prompt-pack provenance note.
- [ ] **T2.4 Landing pages:** rewrite introduction.md (hub; one first-step name; cast
  out) and comparison.md (data-plane warning gets a forward pointer that flips to
  operate/security.md in Phase 3).
- [ ] **T2.5 Upgrades home:** move upgrading.md → `operate/upgrading-to-1.0.md`, write
  `operate/upgrades.md` (standing policy; absorbs support.md's rename-policy text
  parked in T1.2). The Operate part header enters SUMMARY with these two pages.
- [ ] **T2.6 Mechanics:** Phase-2 redirect entries; README/examples link sweep;
  SUMMARY gains Get Started / Concepts / Build with AI parts in final order.
- [ ] **T2.7 Acceptance:** build + lint green; `docs/src/tutorials/` and the old
  getting-started names are gone from SUMMARY; casts still play
  (`first-service.md` references `../casts/quickstart.cast`).

### Phase 3 — Operate (PR 4)

- [ ] **T3.1 Dissolve the eight features/* pages** per the proposal's migration map
  into: production-checklist, docker, kubernetes (move), cluster, security,
  monitoring, traces, failure-handling, backup-restore, audit-logs — plus the
  reference-page fragments already owned by Phase 1 pages (rate-limit/backpressure/
  cache/dedup → channel-config; metrics table → metrics.md, which Phase 1 stubbed from
  observability.md — reconcile, don't duplicate).
- [ ] **T3.2 Topology:** environments.md → cluster.md (+ dev-vs-prod table into
  how-orion-works); packages.md → concepts/packages.md + operate/promotion.md;
  promotion.md also absorbs the admin-api essay parked in T1.1; add the
  ORION_ADMIN_TOKEN note and the mid-apply failure-modes subsection.
- [ ] **T3.3 Troubleshooting:** write it symptom-first; the quarantine section is now
  fully specified by §2.3 (triggers table, 404-vs-503 nuance, /health degraded shape,
  DLQ replay, recovery-by-reload, no-metric caveat).
- [ ] **T3.4 Deletions:** characteristics.md and its d3 CDN dependency; overview.md if
  not already moved in T2.2.
- [ ] **T3.5 Mechanics:** Phase-3 redirect entries (largest block); flip the two
  forward links from Phase 2 (first-service Next Steps, comparison warning) to
  operate/security.md; README/examples link sweep.
- [ ] **T3.6 Acceptance:** build + lint green; `docs/src/features/`,
  `docs/src/architecture/`, `docs/src/topology/` directories are gone;
  `git grep -l "d3js.org\|cdn" docs/` → 0.

### Phase 4 — Build + Guides (PR 5)

- [ ] **T4.1 Build how-tos:** workflows, channels (verb-first sections backed by
  channel-config), connectors, testing (cli-setup's parked bottom half), versioning
  (single owner of import/export).
- [ ] **T4.2 Guides:** worked-examples (setup-first fix, `{{#include}}` from
  examples/packages, honest simulated-effect notes), workflow-patterns (channel_call
  per D3), ci-cd (replaces the broken curl-loop example — new GH Actions workflow
  built on `orion-server package` verbs), kafka-channels per D2.
- [ ] **T4.3 Mechanics:** `tutorials/use-cases.html` redirect; final README/examples
  link sweep; SUMMARY reaches its final §3 shape.
- [ ] **T4.4 Acceptance:** build + lint green; every `{{#include}}` resolves (lint
  covers it); the ci-cd guide's workflow YAML is syntax-checked
  (`actionlint` or `gh workflow view --yaml` dry parse if available).

### Phase 5 — Style sweep (PR 6, mechanical but wide)

- [ ] **T5.1** Page-by-page pass enforcing proposal §6: sentence surgery, admonition
  conversion, "Use for:" selector lines, Next-Steps blocks, one-altitude checks,
  duplicate-block removal.
- [ ] **T5.2** Terminal greps all green (see T6.1 list) — including the
  forbidden-internals and review-ID guards.
- [ ] **T5.3** Full-book read-through in SUMMARY order (the llms-full.txt file is the
  convenient single-file artifact for this) checking journey continuity for the three
  personas: evaluator, new builder, operator-going-to-prod.

### Phase 6 — Tooling (built during Phase 1, listed separately for clarity)

- [x] **T6.1 docs-lint CI step**: ✅ done as `docs/lint.sh` + a step in the
  ci.yml book job. Deviations: pure bash (no lychee dependency yet — anchor
  validation is the one gap, listed as a Phase 5 follow-up); later-phase
  guards are gated by a `DOCS2_PHASE` variable in the script (internals guard
  activates at ≥3, SUMMARY↔llms parity at ≥4); the review-ID guard exempts the
  1.0 upgrade guide until its Phase 2 restructure. Bonus invariant beyond the
  plan: every hard site link in README/examples/llms.txt must resolve to a
  live page **or** a book.toml redirect entry. Original spec:
  1. `mdbook build docs` (already present — catches missing SUMMARY files via
     `create-missing = false`, and bad redirect config via unknown-key errors).
  2. Link check over `docs/book/**/*.html` with lychee (internal + anchors;
     external links cached/limited).
  3. `{{#include}}` path validation: grep sources for `{{#include ` and `test -f`
     each target (mdBook only warns on missing includes).
  4. llms.txt parity: regenerate from SUMMARY.md and `git diff --exit-code`.
  5. Guard greps:
     - `git grep -nE "\((K|R|F|N|S)[0-9]+\)" docs/src` → 0 (review IDs)
     - `git grep -n "46 tools\|6,000" docs/src README.md` → 0
     - function-count "18" only in `reference/functions.md`
     - forbidden internals outside `reference/design-notes.md`:
       `Arc<RwLock`, `tokio::sync::mpsc`, `apply_guards`, `CatchPanicLayer`,
       `arena-mode`
- [x] **T6.2 llms.txt generator:** ✅ done, **recast**: llms.txt turns out to be a
  curated index (hand-written one-line descriptions per entry), so blind
  generation from SUMMARY would destroy its value. Instead: llms.txt stays
  curated; lint check 3 guarantees every URL in it stays live-or-redirected,
  and lint check 8 (gated to Phase ≥4, when the ToC is final) enforces that
  every SUMMARY chapter has an llms.txt entry. No generator script needed.

---

## 6. Sizing and sequencing

| Phase | PR | Pages touched | Size | Parallelizable? |
|---|---|---|---|---|
| 0 | 1 | 6 docs + 2 code comments | S | — |
| 1 | 2 | ~16 (3 moves, 9 new, 4 revised) | **XL** | After it merges, 2/3/4 could proceed in parallel branches, but sequential is safer for SUMMARY/redirect conflicts |
| 2 | 3 | ~15 (splits, 6 concepts, 3 AI, 2 landing, 2 upgrade) | L | — |
| 3 | 4 | ~15 (10 operate, topology splits, deletions) | L | — |
| 4 | 5 | ~9 (5 build, 4 guides) | M | D2/D3 example work can run ahead in its own PR |
| 5 | 6 | all 56, edits only | M (wide, shallow) | — |

Phase 1 is deliberately the heaviest: it front-loads the one-owner-per-fact
foundation. If it proves too large for one review, split it as PR 2a (moves +
mechanics + CI) and PR 2b (the nine new reference pages).

## 7. Risks and mitigations

- **SUMMARY/redirect merge conflicts** between phases → phases are sequential PRs;
  redirect table grows append-only per phase.
- **Interim cross-links to not-yet-existing pages** → never link forward to a missing
  file (`create-missing = false` errors the build); use the two named forward-link
  swaps (T2.1, T2.4 → flipped in T3.5) and the two parked-content markers (promotion
  essay T1.1, rename policy T1.2) instead. Grep for `TODO(docs2)` markers before
  closing each phase.
- **Anchor drift under mdBook 0.5** (heading IDs are lowercased) → lychee anchor
  checking in T6.1 catches broken fragments; fragment redirects only for the three
  known external ones.
- **Tests that pin doc paths or counts** → before each rename phase, run
  `git grep -l "docs/src" crates tests` and update any test fixtures that reference
  moved files (the config-reference parity test keys off `config.toml.example`, not
  the page path, but verify per phase).
- **examples/README.md vs examples.md ownership inversion** (T2.1 pulls walkthrough
  content into the docs) → examples/README.md shrinks to a pointer at the docs page in
  the same PR; don't leave two owners.
- **Kafka guide slipping** (D2) → kafka-channels.md is the only §3 ToC entry allowed
  to land later; SUMMARY gains it when the page lands (no placeholder file).
- **Search/SEO churn** → redirects cover it; the deploy is atomic per merge to main.

## 8. Definition of done

1. SUMMARY.md matches proposal §3 (± the D2-gated Kafka guide).
2. All 31 original pages accounted for exactly as the migration map says; old URLs
   redirect; the 61 inbound links resolve.
3. docs-lint (T6.1) green in CI and enforced on PRs.
4. The five §1.6 contradictions are unrepresentable: each fact has one owner, and the
   guard greps hold.
5. A cold read of Get Started (install → first service → first connector → test &
   promote) succeeds end-to-end against a locally built server, verified once per
   release thereafter by the existing e2e suite's doc-include coupling.
