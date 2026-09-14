# Documentation system

A standard for how documentation is organised, how a page is built, and how it
sounds. It is product-agnostic by design: it describes the system a
documentation set follows, not any product the set documents.

---

## 1. Scope

### 1.1 What this document covers

The content model (2), where pages live (3), the journeys they serve (4), the
template each page type follows (5), the voice they are written in (6), the
code samples (7), the callouts (8), the tables and visual material (9), the
freshness and machine-readable furniture (10), the gates a page and a set pass
(11), and how all of it is enforced (12).

### 1.2 How to read a rule

Every rule is an imperative. Where the reason is not obvious from the rule, the
rule names the failure it prevents, so a set can tell whether a deviation costs
it anything.

A rule states a behaviour, not a preference. When two rules could both apply,
the more specific one wins, and the page's type (2.1) is what decides which is
more specific.

### 1.3 The numbering is stable

Linters, continuous-integration jobs and contributor guides cite these section
numbers. Add a subsection rather than renumber an existing one, and never
reuse a number for a different rule.

### 1.4 What a set decides for itself

This document leaves five decisions to the set that adopts it:

- the navigation axis, and the spine that follows from it (3.1, 3.2);
- the fixed site-wide order of tabs (7.4);
- the canonical example namespace, identifiers and running example (7.2);
- the diagram notation (9.2);
- the tools that enforce each layer (12).

Those decisions, and every deviation from this document, are recorded in the
set's contributor guide, each with what it deviates from and why. A deviation
that is written down is a decision; one that is not is a defect.

---

## 2. Content model

### 2.1 The types

Every page has exactly one type. The type decides the page's template
(section 5), its voice register (section 6) and where it lives (section 3).
Four types cover the four situations a reader can be in. Two more cover the
first fifteen minutes and the operating life of a product, which the first four
otherwise split between a tutorial and a how-to.

| Type | The reader's situation | The question it answers | Must contain | Must not contain |
|---|---|---|---|---|
| **Quickstart** | Has never run the product. Wants proof it works, in minutes. | "Can I see this work?" | Prerequisites, numbered steps, expected output, one artefact in the product's own language, a handoff | Options, alternatives, explanation beyond one sentence per step |
| **Tutorial** | Has run the quickstart. Wants to learn by building something bounded. | "Can you teach me to…?" | Learning objectives, a sequence with no forward references, a recap | Reference tables, production advice, every option |
| **Concept** | Needs a mental model before, or instead of, doing. | "What is this, and why does it work this way?" | A one-sentence definition, the problem it solves, how it works, a minimal complete example, its boundaries | Step-by-step procedures, exhaustive parameter lists |
| **Guide** (how-to) | Knows the basics. Has a specific task. | "How do I…?" | Purpose sentence, prerequisites, steps, a verification step, variants as tabs, a handoff | Teaching, background explanation, unrelated options |
| **Reference** | Is working and needs an exact fact. | "What exactly is…?" | Signature or synopsis, every parameter with type, requiredness and default, returns, errors, one example, caveats | Instruction, persuasion, narrative |
| **Troubleshooting** | Something is wrong. Has a symptom, not a cause. | "Why is this happening and how do I fix it?" | Symptom-titled entries, cause, fix, how to verify the fix | New concepts, marketing, anything not tied to a symptom |

Four supporting page kinds serve navigation and time rather than a reader's
task: the **hub** (a section's landing page), the **release note**, the
**migration guide**, and the **glossary**. They have templates in section 5
but are not content types; a hub introduces pages of one type, and a release
note or migration guide is reference-shaped.

### 2.2 Classifying a page

Ask two questions:

1. Is the reader **doing** something or **understanding** something?
2. Is the reader **acquiring** a skill or **applying** one they have?

Doing + acquiring is a tutorial (or, in the first minutes, a quickstart).
Doing + applying is a guide. Understanding + acquiring is a concept.
Understanding + applying is reference. A page that cannot answer both
questions with one word each is two pages.

The tiebreak between tutorial and guide is the most common mistake in product
documentation. A tutorial's success is that the reader learned; a guide's
success is that the task is done. If the page would be useless to a reader who
already understands the product, it is a tutorial.

### 2.3 One page, one type

- Do not mix types on a page. When a guide needs background, link the concept
  page in one sentence. When a concept needs a procedure, link the guide.
  Crossing that boundary is the single largest structural fault in product
  documentation.
- A page that grows a second type is split, and the split is announced by two
  clear titles, not by a heading in the middle.
- Reference describes and only describes. A reference page that starts
  instructing has borrowed a guide's job; move the instruction.

---

## 3. Information architecture

### 3.1 One navigation axis per level

- Choose the axis of the top level and state it in the contributor guide.
  Two candidates exist, and both are compatible with the content model:
  - **Type-first** for a single product: the top level is the content model
    itself (Get started, Concepts, Guides, Reference, …).
  - **Area-first** for a platform of several products: the top level is the
    product area, and *every* area repeats the same type skeleton inside it.
- Never mix axes at one level. A sidebar with a lifecycle stage, a product
  component, an edition, a persona and a document type as siblings has no
  axis, and readers cannot predict where a page is.
- A persona or an edition is a filter or a badge, never a top-level section.
  Route operators and developers through the type skeleton (an operator reads
  Operate guides and the configuration reference), not through a separate
  tree that duplicates concepts.

### 3.2 The standard spine

For a single product, the top level is six sections in this order. Each has
a one-line promise that appears on the landing page and never changes.

| Section | Promise | Types inside |
|---|---|---|
| **Get started** | See it work, then learn how to think about it. | Quickstart, tutorial, installation |
| **Concepts** | Understand what each part is and why it behaves as it does. | Concept |
| **Guides** | Complete one task at a time. | Guide |
| **Operate** | Run it in production: deploy, configure, secure, observe, upgrade, recover. | Guide, troubleshooting |
| **Reference** | Look up the exact contract. | Reference |
| **Releases** | What changed, what breaks, how to move. | Release notes, migration guides, versioning policy |

A platform repeats Concepts, Guides, Operate and Reference inside each area
and keeps Get started and Releases global.

### 3.3 Depth and size

- At most three levels below the top-level section: section → group → page.
  Past three, a reader can no longer predict where a page is, and the sidebar
  stops being a map.
- A hub introduces at most seven children in prose, and introduces them
  rather than listing them. A section that outgrows seven pages gets groups;
  a group that outgrows fifteen is two groups.
- A reference page does not exceed heading level H4. A page that repeats one
  heading template many times (one per backend, one per provider) is split
  into one page per instance with a shared parent.

### 3.4 Ordering inside a section

- **Get started, Concepts, Guides, Operate:** pedagogical or lifecycle order.
  Each page may assume the pages before it and never the pages after it, so a
  reader can start at the first page and work to the last without looking
  ahead for a definition. Lifecycle order for a task section is the task's own
  order: create, connect, change, retire. Alphabetical order in a guides
  section is a failure — it puts the rarest task next to the first one.
- **Reference:** the structure of the machinery, then alphabetical within a
  kind.
- **Releases:** newest first.

### 3.5 Skeleton identity

- Every sibling tree has the same section sequence with the same labels. A
  reader who has learned one tree has learned them all.
- Concepts are written once, outside the sibling trees, and linked from each.
  Duplicating prose across trees is acceptable only when generated from one
  source.

### 3.6 One title per page

- The H1, the sidebar label, the browser title and the breadcrumb are the
  same string. If the sidebar must be shorter, the shortening rule is written
  down and mechanical (drop the leading "How to", keep the rest) so a reader
  can match the two.
- Titles are not written for search engines. An SEO suffix belongs in the
  page description metadata, never in the H1.
- No two pages in a set share a title. A reader arriving from search sees the
  title and nothing else; two pages with one title are indistinguishable.

### 3.7 Where reference lives

- Reference is a section of the same site, in the same shell, one hop from
  any guide that uses it. A separate reference site, or a bridge page that
  only links out, costs the reader the hop that reference exists to save.
- One home per reference kind: one API reference, one configuration
  reference, one CLI reference, one language or expression reference. Never a
  product-level summary that duplicates the global one.
- Generated reference (from a schema, a source table, a plugin manifest)
  carries the version and date it was generated from, and links back to the
  concept and guide pages that use it. A one-way link from the documentation
  into a generated catalogue leaves the catalogue with no route home.

### 3.8 Fixed homes for the rest

- **Versions and upgrade policy:** Releases, not a community section or a blog.
- **Release notes:** Releases, one entry per version, filterable by major and
  minor, with a feed.
- **Migration guides:** Releases, one per breaking version, with one sub-page
  per breaking change.
- **Glossary:** one page under Concepts, linked from every first-use term
  that has no concept page of its own.
- **Changelog for a component, SDK or plugin:** with that component, dated,
  linking to the full release note.

---

## 4. Reader journeys

### 4.1 The three journeys

Design the site so each journey is a straight line through the spine.

| Journey | Reader | Path | Success |
|---|---|---|---|
| **Evaluate** | Has not decided to use the product | Landing → what it is (one sentence) → use cases → quickstart | A working result in under fifteen minutes, or a clear reason to stop |
| **Build** | Is building something | Quickstart → tutorial → concepts and guides, alternating → reference | A shipped feature, with every fact looked up in one hop |
| **Operate** | Runs it for others | Install → configure → secure → observe → upgrade → troubleshoot | A production deployment whose every setting the operator can explain |

A page belongs to at least one journey. A page on no journey is either
reference (reached by search) or does not belong.

### 4.2 The landing page

- Opens with one sentence saying what the product is, in the reader's terms,
  with no adjectives. Name the category and what the reader does with it; cut
  every word that only raises the temperature.
- The primary call to action is the quickstart. It is the first link.
- Offers entry points by intent, in the reader's words: "I'm new here",
  "I want to try it", "I'm upgrading", "I'm operating it". Intent beats
  persona; a reader knows what they want before they know what they are.
- Introduces the spine: six sections, six promises (3.2).
- Contains no feature list, no testimonials, no comparison. Positioning
  lives on the marketing site.

### 4.3 The quickstart

- States the time to complete in the first paragraph, measured with a new
  reader, not estimated.
- Has a Prerequisites block before step one, with versions.
- Has at most ten numbered steps, each a single action with a verb-first
  heading.
- Produces one artefact in the product's own language (a definition file, a
  service, a request) that the reader can read. A quickstart that only runs
  a container and opens a UI has not shown the product.
- Shows the expected output after every command that produces one, and one
  verification the reader performs themselves.
- Ends with three things in this order: what just happened (one paragraph,
  the mental model behind the result), a marker that the happy path is
  complete ("Congratulations" is fine), and a handoff to the tutorial and to
  the first concept.
- Contains no options, alternatives, or "if you prefer" branches beyond the
  choice of language or platform, which is a tab (7.4).

### 4.4 Every page is an entrance

- Assume the reader arrived from search. The first paragraph states the
  page's scope and what the reader will have at the end, in one to three
  sentences.
- Do not repeat the product definition on every page. A page-level scope
  sentence is the right size; a paragraph of product boilerplate is not — it
  costs a screen before the page's own first fact.
- Do not depend on the previous page's state without saying so. "This guide
  assumes you completed the quickstart and have the server running on port
  8080" is the form.

### 4.5 Handoffs

- Every non-reference page ends with a handoff: three to five links, each with
  a one-line description of what the reader gets there. A bare link list is
  not a handoff.
- Every guide links, by name, the reference pages for the things it used, at
  the end.
- Every reference page links the concept that explains it and the guide that
  uses it, in its first paragraph.
- Every concept page links the guides that apply it and the reference that
  specifies it, at the end.
- Link on first mention, at the point of use, to the most specific target
  (an anchor, a parameter, a type's own page), never to a section index.

### 4.6 The disclosure ladder

Use the least hidden device that works. From most to least visible:

1. **A separate page**, when the material has its own reader.
2. **A tab on the same URL**, for alternatives that are equivalent (language,
   platform, UI versus API). The URL carries the choice so the tab state is
   shareable.
3. **An "Optional:" prefix on a heading**, for a step some readers skip.
4. **Material placed after the happy-path marker**, for customisation that
   follows success.
5. **A collapsible, closed by default, titled as the question it answers**
   ("Is an updater always preferred?"). Never for a required step, a
   prerequisite, or a warning.
6. **A callout**, last, and only under section 8's rules.

---

## 5. Page templates

### 5.0 The common frame

Every page carries, in this order:

1. Breadcrumb, computed from the navigation tree.
2. **Title** (H1), sentence case, matching the sidebar label (3.6).
3. **Description**: one sentence, present tense, stating the page's purpose.
   It is the subtitle on the page, the sidebar tooltip, the search snippet
   and the metadata description, from one source.
4. **Scope paragraph**: one to three sentences (4.4). For tutorials and
   quickstarts, a "You will learn" or "What you'll build" box instead.
5. The body, per type below.
6. The handoff (4.5).
7. Page furniture: "Previous / Next" within the section; "Was this page
   helpful?" with reasons (10.4); "Edit this page"; the freshness stamp
   (10.2); "Copy as Markdown".
8. A right-hand "On this page" table of contents on wide screens, built from
   H2 and H3.

### 5.1 Quickstart

1. Time to complete and what you will have at the end.
2. Prerequisites, with versions.
3. Steps 1..N. Each: verb-first heading, one action, the command or file,
   the expected output.
4. Verify: one action the reader performs to prove it worked.
5. What just happened: one paragraph.
6. Completion marker.
7. Next steps: the tutorial, the first concept, the installation guide.

### 5.2 Tutorial

1. "You will learn": three to six bullets.
2. Prerequisites, including which tutorial or quickstart precedes this one.
3. Sections in dependency order, each building one thing, each with a
   complete runnable state at its end. Number them "Step N:" only when the
   reader must do them in order and in one sitting.
4. Recap: bullets restated as rules the reader can now apply.
5. Exercises with hidden solutions, when the product can be exercised
   without infrastructure. Otherwise omit; do not fake it.
6. Next: the following tutorial, or the concept chapter that deepens this one.

### 5.3 Concept

1. Definition: one sentence, the noun and what it is for.
2. The problem it solves, from the reader's side, before any mechanism. Name
   what the reader would otherwise have to build or reason about themselves.
3. How it works: the mental model, then the minimal complete example in the
   product's language, then the properties or parts as a short table.
4. Boundaries: what it is not, its limits with numbers, when to use the
   neighbouring concept instead. A comparison table when two concepts are
   commonly confused.
5. Handoff: the guides that apply it, the reference that specifies it.

### 5.4 Guide (how-to)

1. Purpose sentence in the description: "Configure alerts that fire whenever
   a workflow fails."
2. Prerequisites: what must already exist, with links.
3. Steps, verb-first, numbered when order matters. Equivalent surfaces (UI,
   CLI, API) are tabs in a fixed site-wide order, not repeated sections.
4. Verify: how the reader knows it worked.
5. Options and variants, after the happy path, each as its own H2.
6. Troubleshooting: the two or three failures this task commonly hits, as
   symptom-titled H3s, or a link to the troubleshooting page.
7. Handoff: the reference pages used, the adjacent guides.

### 5.5 Reference

One skeleton for every reference kind. Sections that do not apply are
omitted, never renamed.

1. **Synopsis**: the signature, the shape, the grammar, or the full default
   configuration.
2. **Description**: what it does, in present-tense declaratives. No
   instruction.
3. **Parameters** (or fields, options, properties): a table with fixed
   columns in a fixed order: name, type, required, default, description. Each
   description is one sentence of nine to twenty words. Enumerated values are
   each listed with one line. Nested objects collapse behind a "Show child
   parameters" control.
4. **Returns** (or outputs, response, exit codes).
5. **Errors**: every error the thing can raise, with the condition.
6. **Examples**: one complete example per common use, each with a one-line
   title.
7. **Caveats**: the surprising behaviours, as bullets.
8. **Compatibility** (or "Since version"): what changed and when, inline
   with the number.
9. **Related**: the concept page and the guide, by name.

Generated reference must render into this skeleton, and must carry "Generated
from version X on date" (10.2).

### 5.6 Troubleshooting

1. Entries titled in the reader's own words, as a symptom, not a cause:
   "My handler runs twice on startup"; "How do I reset my root password?".
   First person or question form; never a noun phrase.
2. Each entry: the symptom as the reader sees it (the exact error text in a
   code block), the cause in one or two sentences, the fix as steps, and how
   to verify.
3. Entries ordered by frequency, not alphabetically.
4. Every error code the product emits has an entry or a reference row with a
   "Make sure:" checklist.

### 5.7 Hub

1. One paragraph saying what the section is for and who reads it.
2. Its children, at most seven, each introduced with one sentence, in the
   order they should be read.
3. For a section with many how-tos, a routing table: "If you need to… |
   Start here".
4. No other content. A hub that grows body text is a concept page in
   disguise.

### 5.8 Release note

One entry per version, structured, newest first:

- Version and date, in an unambiguous date format.
- A one-sentence summary that reads as a headline.
- Changes grouped by area, each a verb-first present-tense line ("Adds…",
  "Removes…", "Fixes…").
- A **Breaking** flag on every entry, present even when false.
- For each breaking change, a link to its migration sub-page.
- A link to the full notes in the repository.

### 5.9 Migration guide

- One guide per breaking version, with a stated baseline ("Start from
  version 1.3").
- A table at the top: change, who it affects, what to do.
- One sub-page per breaking change, each with before, after, and the
  command or check that finds affected definitions.
- The upgrade procedure page (in Operate) links here and to the release
  note; the three never contradict each other.

### 5.10 Glossary

- Every product noun, ten to twenty words each, one URL per term, linked from
  the first use on every page that has no concept page for it.
- The product's core nouns are always in it. A glossary that omits the
  product's own primary noun has been written for the people who already know.
- A single "conceptual model" page, listing every noun and how they relate,
  precedes the glossary when the product has more than ten nouns.

---

## 6. Voice

### 6.1 Person, mood, tense

- Address the reader as **you**. Never "the user" for the reader.
- Use **we** only for a recommendation or an opinion ("We recommend…", "We
  are working on…"). Never for what the product does: the product acts under
  its own name or as "the server", "the engine", "the CLI".
- **Imperative** for every step: "Create a handler." Not "You should create a
  handler" and not "The handler is created".
- **Present tense** for every behaviour: "The server sends an
  acknowledgement." Never "will send".
- **Active voice** unless the actor is unknown or irrelevant.
- Put the condition before the instruction: "If the request fails, retry
  with backoff." Not "Retry with backoff if the request fails".

### 6.2 Sentence and paragraph

- One idea per sentence. Target an average of fifteen to eighteen words;
  treat twenty-five as the ceiling and rewrite anything longer. The ceiling is
  what stops a forty-word sentence reaching a reader who is halfway through a
  task.
- One to three sentences per paragraph. A fourth sentence becomes a list, a
  table, or a new paragraph.
- Lists for parallel items, numbered only when order matters.
- Tables for anything with two or more attributes per item.
- Write for scanning first, reading second: the first words of a paragraph,
  a bullet or a heading carry its meaning.

### 6.3 Headings and titles

- Sentence case for every heading, including page titles. Product nouns keep
  their capitals.
- No end punctuation on a heading.
- Heading form by type: verb-first for procedures ("Register the service");
  noun phrase for concepts and reference ("Durable timers"); the reader's
  sentence for troubleshooting ("The endpoint answers 404 after activation");
  "Step N: verb" only in quickstarts and tutorials.
- Do not skip heading levels. Do not go past H4.
- A heading is not a sentence continued from the paragraph above it.

### 6.4 Terms and definitions

- Define each term once, at first use, in the reader's language, before using
  it. Mark the term in italics on that first use.
- Teach the mental model before the mechanism: what the thing is for, then
  what it is called, then how it is spelled.
- One name per concept, held constant across the site. Product concepts are
  capitalised consistently as proper nouns when they are product-specific and
  lowercase when they are ordinary words.
- Expand an acronym at first use on every page: "virtual machine (VM)".
- The concept page is the canonical home of a definition. Every other page
  links to it rather than redefining it.

### 6.5 Warnings, limits, prohibitions

- Form: "Don't X. Reason." Flat, present tense, no exclamation mark.
- Limits carry the number and the scope: "up to 16 endpoints per account",
  "over 100 tasks per workflow". Never "a large number".
- Consequence first when the consequence is loss: "Running two instances can
  cause split-brain. Ensure only one instance is running when restoring."
- Reserve the Warning label for data loss, security, or irreversible actions
  (8.2). Everything else is a Note or plain prose.
- Name the status of anything unstable, inline with the version: "Opt-in and
  disabled by default; its configuration may change".

### 6.6 Typography

- **Bold**: UI labels ("Click **Create**"), and at most one load-bearing
  phrase in a paragraph. Never a whole sentence.
- *Italic*: a term on first definition, and a stressed word where the stress
  changes the meaning. Nothing else.
- `Code`: identifiers, file names, paths, commands, flags, values, status
  codes, keys, environment variables. Everything a reader could type or read
  in a terminal.
- Keys and placeholders: `<angle-brackets>` for values the reader supplies,
  uppercase environment variables for secrets.

### 6.7 Words and habits to avoid

- **Ease words**: simple, simply, easy, easily, just, quickly, obviously, of
  course. They tell the reader who is struggling that the fault is theirs.
- **Throat-clearing**: "In this guide", "It's worth noting that", "Please
  note", "Note that", "As mentioned above".
- **Marketing adjectives**: powerful, seamless, robust, blazing, scalable,
  enterprise-grade, best-in-class, leverage.
- **Future tense for behaviour** ("will return").
- **"Please"** in instructions.
- **Exclamation marks**, except the completion line of a tutorial.
- **Directional language** ("above", "below", "on the right") — name the
  section or the element instead.
- **Ellipses for omitted code** — use a comment in the language.
- **Latin abbreviations** (e.g., i.e., etc.) — write "for example", "that
  is", and finish the list.

### 6.8 Contractions

Contractions are allowed in quickstarts, tutorials, guides and concepts,
where the register is a colleague explaining. They are not used in reference
descriptions, parameter tables, error text or warning callouts, where the flat
form reads as a contract. If the documentation will be translated, remove them
everywhere and record the decision.

### 6.9 Marketing, humour, hedging

- No marketing inside the documentation. The landing page gets one
  positioning sentence; nothing else does.
- Humour is permitted only in tutorials and only when it does not cost a
  word the reader needs. It is never used in reference, operate or
  troubleshooting pages.
- Hedge only for genuine uncertainty, and say what is uncertain: "Webhook
  endpoints might occasionally receive the same event more than once". Never
  hedge a fact the product guarantees.
- No competitor comparisons on instructional pages. Comparisons are
  evaluation content with their own section.

### 6.10 Global and inclusive

- Write for a global audience: no idioms, no pop-culture references, no
  regional date formats.
- Use "they" for a generic person. No "he or she".
- Use "primary/replica", "allowlist/blocklist", "stop responding".
- Focus on people, not conditions; no ableist figures of speech.
- Names and example data reflect more than one culture and are never real
  people, real keys, or real addresses (7.2).

---

## 7. Code samples

### 7.1 Completeness

- Every sample is complete for its scope: a request that can be sent, a file
  that can be saved, a command that can be run. A fragment is marked as a
  fragment with a comment in the language ("# … the rest of the file is
  unchanged") and gets no copy button.
- Every sample that produces output shows the output, in a separate block,
  labelled as output.
- Commands are shown without a shell prompt (`$`) so they copy cleanly;
  command and output are never in one block.
- Lines wrap at 80 characters.

### 7.2 Safety and placeholders

- Secrets are placeholders in one site-wide convention, with a comment that
  says not to put keys in code.
- Example hosts, IPs, e-mails and names are reserved-for-documentation values
  (`example.com`, `203.0.113.0`, `user@example.com`).
- One canonical example namespace, organisation and set of identifiers is
  used in every sample on the site, so readers recognise the running example.

### 7.3 Commentary

- One sentence before each block says what the block does or what the reader
  should notice, and ends with a colon. It does not repeat the code.
- Comments inside code explain a line the reader could not infer; they do
  not narrate. The explanation of a command's flags goes in prose or a
  collapsible, not in the command.
- One sentence after a block says what happens next or what to check, when
  the block is a step.

### 7.4 Variants

- Language, platform and surface variants are tabs on one URL (4.6). The tab
  order is fixed for the whole site and written in the contributor guide. One
  choice gets one mechanism: a set that routes by language in three places
  routes it the same way in all three.
- Every tab has the same steps, the same headings and the same commentary;
  only the code differs. A variant that needs different prose is a different
  page.
- File blocks carry a filename label.

### 7.5 Provenance

- Samples are sourced from a repository of examples that is built and run in
  continuous integration, and included by anchor, not pasted. When that is not
  possible, the sample is a test fixture that CI executes.
- A sample that cannot be executed anywhere is marked as illustrative in its
  introductory sentence.

---

## 8. Callouts

### 8.1 Vocabulary

Four labels, and only four:

| Label | Use | Form |
|---|---|---|
| **Note** | A fact the reader needs now that does not fit the flow | One or two sentences |
| **Tip** | A shortcut or a better way, optional | One or two sentences |
| **Warning** | Data loss, security exposure, or an irreversible action | "Don't X. Reason." or "X causes Y. Do Z." |
| **Deprecated** | The thing described is going away | What replaces it, and when |

Release channel, edition and stability (preview, experimental, enterprise
only, since version) are **badges** on the heading or metadata, not callouts
(8.4).

A large label vocabulary is never applied consistently: the labels blur, and
the one that means "you can lose data" stops being read. Four is the number
that stays learnable, and it is what gives Warning its weight back.

### 8.2 Rules

- Every callout is labelled. An unlabelled blockquote is a callout the reader
  cannot weigh.
- One trap per callout. A callout with two topics is two callouts or, more
  likely, a paragraph.
- At most three callouts per page and never two adjacent. If a page needs
  more, its structure is wrong: the traps belong in a Caveats section (5.5)
  or a troubleshooting entry (5.6).
- A callout never carries a required step, a prerequisite, or the only
  statement of a limit. Those are body text.
- Warnings appear before the step they warn about, not after.

### 8.3 Placement

- A callout follows the paragraph it qualifies. It does not open a page and
  does not open a section.
- Callouts are not used to talk to a different reader than the page's
  (operators on a developer page, agents on a human page).

### 8.4 Badges

- Edition, stability and version gates are one visual device, applied the
  same way everywhere: a badge beside the heading of the gated thing, and the
  same badge in the reference table row. One fact gets one marking; a set that
  marks an edition four ways has taught readers to trust none of them.
- A gated page says so in its first paragraph as well, in prose, so the fact
  survives copying and machine reading.

---

## 9. Tables, diagrams, screenshots, video

### 9.1 Tables

- Parameter tables use fixed columns in a fixed order (5.5). A site has one
  parameter-table shape, and one spelling for each controlled value in it.
- Routing tables ("If you need to… | Start here") are the hub's tool for
  sections with many pages.
- Comparison tables carry the concepts as columns and the distinguishing
  properties as rows.
- No merged cells, no empty header cells. A wide table scrolls in its own
  container; the page never scrolls sideways.

### 9.2 Diagrams

- A diagram is added when words and a code sample cannot carry the idea,
  and is removed when they can.
- One notation across the site: the same shape for the same kind of thing,
  the same arrow for the same relation.
- Every diagram has alt text that states what it shows and is referred to by
  name in the prose, never by position.

### 9.3 Screenshots

- Prefer a command and its output to a screenshot of a UI. Use a screenshot
  when the reader must recognise a screen.
- Alt text is an inventory of what is on the screen: controls, labels, state.
- A screenshot is dated by the version it shows, cropped to the relevant
  region, and re-taken when that region changes. No annotation arrows that
  the alt text does not explain.

### 9.4 Video

- Video is optional and never the only carrier of a step or a fact. The text
  stands on its own, and the video is an alternative to it, not a part of it.
- A video is placed after the text it accompanies, with a one-sentence
  description and a duration.

---

## 10. Freshness, versioning, feedback, machine readers

### 10.1 Versioning

- One version selector for the documentation set when versions differ in
  content; otherwise, none, and version gates are inline: "Since 1.7.3,…".
- Previous major versions are kept as whole trees, not as annotations in the
  current tree.
- The versioning policy is a Releases page.

### 10.2 Freshness

- Every authored page carries "Last verified <date>", set by a person who
  ran it. Every generated page carries "Generated from <version> on <date>".
  The two stamps have two meanings and both are documented on one page in the
  contributor guide. There is no third stamp.
- A page whose "Last verified" is older than the product's last minor
  release is flagged in the build.
- An index page's stamp is the newest of its children, never older than any
  of them. An index dated years before the page beneath it tells the reader
  the wrong thing about both.

### 10.3 Accountability

- Every page has "Edit this page" and a route to the source.
- Every restructure ships redirects. A stale link inside the site is a build
  failure, not a reader's problem.
- The style rules in this document are enforced by a linter in continuous
  integration (12).

### 10.4 Feedback

- Every page has "Was this page helpful?" with at least one structured reason
  on "No" ("Code sample is wrong", "Missing information", "Out of date").
- Feedback lands in the same tracker as the documentation source, tagged with
  the page.

### 10.5 Machine readers

- Every page is served as Markdown at its own URL, and the set publishes
  `llms.txt`.
- Anything written *to* an agent (tool hints, routing preferences, "do not
  use X unless asked") lives in `llms.txt` or in agent-only metadata, never
  in the reader's page. A page argues for one thing: the reader's task.
- The Markdown twin is the same document, not a flattened variant with
  duplicated headings. If tabs exist, the twin carries every tab under its own
  heading, once.

### 10.6 Links

- Internal links are relative, resolved at build time, and checked in CI.
- External links are checked on a schedule and replaced or removed when
  dead.
- Link text says where the link goes; never "here", "this page", "click
  here".

---

## 11. Quality gates

### 11.1 A page is done when

1. It has exactly one type (2.1), and its template's sections are present in
   order (5).
2. Its title, sidebar label and breadcrumb are one string, in sentence case
   (3.6, 6.3).
3. Its description is one present-tense sentence, and its scope paragraph is
   one to three (5.0).
4. A reader arriving from search knows in the first paragraph whether this is
   their page (4.4).
5. Every term is defined at first use or linked to its concept page (6.4).
6. Every code sample is complete for its scope, has a one-sentence
   introduction, shows its output, and contains no real secret (7).
7. Variants are tabs in the site's fixed order, with identical prose (7.4).
8. Callouts are labelled, at most three, none adjacent, none carrying a
   required step (8).
9. Every limit has a number and a scope; every warning has a reason (6.5).
10. The average sentence is under eighteen words and no sentence exceeds
    twenty-five (6.2).
11. No word from 6.7 appears.
12. Headings are sentence case, verb-first for procedures, and skip no level
    (6.3).
13. The page ends with a handoff of three to five described links (4.5), and
    a guide's handoff names its reference pages.
14. It links the concept that explains it and the reference that specifies
    it, and they link back (4.5).
15. It carries a freshness stamp and an edit link (10.2, 10.3).
16. Its Markdown twin renders the same content once (10.5).
17. Images have alt text that states content, and nothing is referred to by
    position (9).
18. A person other than the author followed it, and for a quickstart or
    guide, the stated time held (4.3).

### 11.2 The set is done when

- The top level has one axis and six sections with promises (3.1, 3.2).
- No section exceeds three levels; no hub introduces more than seven
  children (3.3).
- Every sibling tree has the identical skeleton (3.5).
- Every reference kind has exactly one home (3.7).
- Every product noun has a glossary entry or a concept page (5.10).
- Every error code has a troubleshooting entry (5.6).
- Every breaking change has a migration sub-page (5.9).
- The three journeys (4.1) can each be walked without leaving the spine.
- The build fails on a broken link, a missing description, a missing stamp,
  a heading in title case, a banned word, or a page without a type (12).

### 11.3 Measures to publish

| Measure | Target | Source |
|---|---|---|
| Time to first success on the quickstart | Under 15 minutes for a new reader | Usability sessions, quarterly |
| Average sentence length | 15 to 18 words | Linter, per page |
| Longest sentence | 25 words | Linter, per page |
| Callouts per page | 3 or fewer | Linter |
| Navigation depth | 3 or fewer | Build |
| Hub children | 7 or fewer | Build |
| Broken internal links | 0 | Build |
| Pages older than the last minor release | 0 | Build |
| "Not helpful" rate per page | Tracked, top ten reviewed monthly | Feedback widget |
| Code samples not executed in CI | 0 | CI |

---

## 12. Enforcement

- **Prose linter**: a Markdown-aware linter carrying a published base style
  plus a house style holding the product's nouns, the banned words (6.7), the
  link text (10.6) and heading case (6.3). Run in the editor, as a pre-commit
  hook and as a pull-request check that annotates changed lines; start
  advisory and tighten to failing over one quarter.
- **Structure linter**: a build step that asserts each page's metadata
  (type, description, last verified), the template sections for its type in
  order, the navigation depth, hub size, and title identity.
- **Link checker**: internal links at every build, external links weekly.
- **Sample runner**: every code block with a runnable language is extracted
  and executed, or included by anchor from a tested repository.
- **Drift tests**: any fact the documentation restates from the product
  (a setting, a name, a default, a route, an error code) is asserted against
  the product in the test suite, so the code is authoritative and the build
  fails when a page disagrees.
- **Redirect map**: a renamed or moved page ships its redirect in the same
  change.
- **Usability script**: a written task list run with new readers each
  quarter, whose results set the quickstart's stated time (4.3).
