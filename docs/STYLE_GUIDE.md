# Orion documentation style guide

This guide governs files under `docs/src`. Edit source files there; do not edit
generated output under `docs/book`.

## Audience and purpose

Write for developers who are evaluating Orion, building a service, or operating
one. Assume general API and command-line experience, but do not assume knowledge
of Orion terms.

Give every page one primary type:

- **Tutorial:** teaches by completing a bounded project.
- **How-to guide:** solves one concrete task for a reader who understands the
  basics.
- **Concept:** explains a mental model, behavior, or trade-off.
- **Reference:** states an exact contract for lookup.
- **Evaluation guide:** helps a reader decide whether or how to use Orion.

The directory communicates that type: `getting-started` contains tutorials,
`concepts` explanations, `build` and `operate` how-to guides, `reference` exact
contracts, and `compare` evaluation guides. If a page needs several types,
split it or link to the page that owns the secondary purpose.

## Voice and language

- Address the reader as “you” in tutorials and how-to guides. Use neutral,
  declarative language in reference pages.
- Lead with the outcome, then the command or contract, then rationale.
- Prefer short, literal verbs. Reserve slogans and rhetorical language for the
  introduction.
- Qualify operational claims. Name the version, topology, or condition behind
  statements such as “zero downtime,” “safe,” and performance figures.
- Define Orion-specific terms on first use and link to the glossary. Avoid
  unexplained “estate,” “ingress,” “closure,” and “keyset paging.”
- Use sentence case for headings.

## Preferred terminology

| Use | Avoid or reserve |
|---|---|
| Orion Console | The Console, Web Console, Orion UI when referring to the product experience |
| data plane / control plane | data-plane / control-plane as nouns; hyphenate only as adjectives |
| CI/CD | CI-CD |
| workflow ID, channel ID | workflow id in prose; use exact wire keys such as `workflow_id` in code |
| dry-run | dry run when used as a noun unrelated to the command |
| Workflow JSON Schema | Workflow Schema, Workflow Reference as link titles |
| instance | estate, unless the complete stored set across versions is specifically meant |

Wire names are never editorialized. If the API field is `workflow_id`, show
`workflow_id` exactly.

## Page openings

The first line must be a unique 110–160 character description comment consumed
by `docs/seo.mjs`:

```markdown
<!-- description: A standalone summary that distinguishes this page in search results and answer-engine indexes. -->
```

Follow the H1 with the page type and audience when the generated directory
label is not specific enough. Tutorials also show:

```markdown
**Tested with:** Orion 1.5.1 · **Last reviewed:** YYYY-MM-DD
```

The tested version must match the workspace version. Update the date only after
running the documented path.

## Tutorials and how-to guides

Use this order when applicable:

1. Outcome and audience
2. Prerequisites and tested version
3. Numbered steps
4. Expected result or verification after every state-changing step
5. Common failure and recovery
6. Repeat-run behavior and cleanup
7. Next steps

Choose one primary interface. Link to CLI, raw HTTP, Console, or AI alternatives
after the primary path succeeds. Do not interleave several equivalent paths.

Never recommend piping a remote script directly into a shell. Download it to a
named file, invite inspection, then run that file.

## Examples and command output

- Prefer complete files from `examples/` via `{{#include}}` or a repository
  link. Say whether a snippet is complete or partial.
- Keep one example domain through a learning path. Orion's primary beginner
  domain is orders.
- Use placeholders in angle brackets, for example `<trace-id>`. Explain which
  values vary.
- Do not include a shell prompt character. This keeps commands copyable.
- After a command that changes state, show a concise expected result or a
  verification command.
- Label intentionally failing commands and say which exit status to expect.
- Explain whether rerunning a command is idempotent and how to clean up created
  containers, volumes, files, and Orion entities.

## Reference pages

Start with a purpose sentence and a searchable index. Use this template for each
function, endpoint, connector, channel block, or configuration group when the
fields apply:

1. Purpose
2. Syntax or path
3. Field table with type, required state, default, and constraints
4. Version applicability, using `**Since:** Orion x.y`
5. Complete minimal example
6. Result shape
7. Errors with HTTP status, stable error code, and corrective action
8. Related concept and how-to links

Put normative behavior before rationale. Label rationale explicitly or move it
to `reference/design-notes.md`. Client code must branch on stable codes and
fields, never prose messages.

## Notes and warnings

- `TIP` offers an optional shortcut.
- `NOTE` clarifies behavior without requiring action.
- `IMPORTANT` states an action needed for success.
- `WARNING` identifies a realistic risk of failure, data exposure, or outage.
- `CAUTION` is reserved for destructive or difficult-to-recover actions.

Do not use a warning merely to emphasize normal prose.

## Links and navigation

- Link to the page that owns a fact instead of restating the fact.
- Use the destination's current title as link text.
- End tutorials and how-to guides with `## Next steps`; reference pages use
  `## Related`.
- Preserve established page paths and fragment IDs. Add redirects before
  moving or renaming a published page.

## Validation

Run both commands before submitting a documentation change:

```bash
bash docs/lint.sh
bash docs/build.sh
```

The lint checks navigation, links and fragments, page descriptions, titles,
tutorial metadata, deprecated terminology, tested versions, and JSON example
files. The build catches renderer and post-processing failures.
