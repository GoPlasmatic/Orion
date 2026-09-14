# Orion documentation style guide

The system this book follows is
[`DOCUMENTATION_STANDARD.md`](./DOCUMENTATION_STANDARD.md): the content model,
the page templates, the voice rules, the callout vocabulary and the quality
gates. The decisions it leaves to each set — the navigation axis, the tab
order, the freshness stamps, the recorded deviations — are in
[`CONTRIBUTING-DOCS.md`](./CONTRIBUTING-DOCS.md).

This page holds what is left: the words Orion uses for its own things. It is
the vocabulary §6.4 asks to be held constant across the site, and it is the
source for `docs/styles/config/vocabularies/Orion/accept.txt`.

## Product nouns

Capitalised, always, because they name this product's concepts:

**Orion**, **Orion Console**, **Plasmatic**.

Lowercase, always, because they are ordinary words for a thing the reader
declares in JSON:

**channel**, **workflow**, **connector**, **plugin**, **model**, **package**,
**task**, **task group**, **step**, **trace**, **occurrence**.

Write "a channel", not "a Channel". The entity is the JSON document; the word
is the word.

## Preferred terms

| Use | Not | Why |
|---|---|---|
| definition | config, spec, manifest | A definition is the JSON a reader posts. `manifest` is reserved for a plugin's `plugin.toml` and a model's `orion:model` document. |
| activate | deploy, publish, promote | `activate` is the status change. `promote` is moving a package between instances, which is a different verb with its own page. |
| dry run (noun), dry-run (the flag) | test run | `--dry-run` and `?dry_run=true` are the wire spellings. |
| the admin API | the management API, the control plane API | `/api/v1/admin/` is the path. |
| the data plane | the runtime API, the invocation API | The pairing is control plane / data plane. |
| quarantine | disable, park, deactivate | A quarantined entity is stored and refused at every ingress. Nothing was deactivated. |
| ingress | entry point, inbound | One word for the four ways work reaches the engine: a request, a Kafka record, a queued trace, a `channel_call`. |
| task function | function, action, operator | `operator` is a JSONLogic operator and nothing else. |
| the engine | the runtime, the core | `the runtime` is the whole product; `the engine` is dataflow-rs inside it. |
| Orion answers `409` | Orion throws a 409, returns a 409 error | The product answers a status. Status codes are in code font. |

## Spelling

Oxford: `-our` endings with `-ize` verbs.

**behaviour**, **colour**, **favour** — and **normalize**, **serialization**,
**recognize**, **organize**, **analyze**.

American technical spellings that are the wire name stay as the wire name:
`normalize` in a function's field table is the field, not a choice.

## Wire names in prose

An identifier that a reader could type is in code font, exactly as the product
spells it: `workflow_id`, `channel_call`, `ORION_SERVER__PORT`,
`POST /api/v1/admin/engine/reload`, `503`.

Prose may say "the workflow ID" when talking about the idea, and must say
`workflow_id` when naming the field. The two never appear in one sentence in
two spellings.

## The running example

One service, everywhere in the book: an orders service, in three tested forms.

| Form | Workflow | Channel | Route | Ships as |
|---|---|---|---|---|
| Quickstart | `quickstart-orders` | `orders` | `POST /orders` | `examples/quickstart.sh` |
| Packaged | `high-value-order` | `high-value-orders` | `POST /high-value-orders` | `examples/packages/high-value-order/` |
| Database-backed | `record-order` on connector `orders-db` | `record-order` | `POST /record-order` | `examples/packages/postgres-orders/` |

The rule in the example logic is "flag an order whose `total` exceeds
`10000`". A page needing a second service uses `notification-routing` on
`POST /notifications`. A page needing a third has too many.

## Related

- [Documentation standard](./DOCUMENTATION_STANDARD.md): the content model, the
  templates, the voice rules and the quality gates.
- [Contributing to the Orion book](./CONTRIBUTING-DOCS.md): the navigation
  axis, the tab order, the freshness stamps and the recorded deviations.
- [Glossary](./src/reference/glossary.md): every product noun, defined once.
