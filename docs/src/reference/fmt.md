<!-- description: The one layout `orion-server fmt` writes: the numbers, the key order of every recognised shape, and the JSONLogic inlining rules — with nothing to configure. -->
# Definition Style (`fmt`)

`orion-server fmt` rewrites definition files to one layout, the way `gofmt`
and `cargo fmt` do. There is no configuration: every file in every tree
reads the same way, the examples in this book are in the same style as your
own, and a style change is a change to Orion rather than to a project's
settings. This page is that style, stated as rules. The command itself is
documented in the [CLI reference](./cli.md#fmt).

## What the formatter changes, and what it never touches

It changes **whitespace**, **string escapes** (re-emitted canonically), and
the **order of known keys in known shapes** (the tables below).

It never changes a value, the spelling of a number (`1.0` stays `1.0`, `1e3`
stays `1e3`), the order of array elements, or the order of keys it does not
recognise — a connector's `config`, an `http_call` body, a case file's
`input` are written back in the order you wrote them. Before anything is
written, the output is parsed again and compared with the input as the
runtime sees it; a difference is reported as a formatter bug and the file is
left alone.

## The numbers

| Setting | Value |
|---|---|
| Line width | 100 columns — a node prints on one line when it fits |
| Indent | 2 spaces |
| Scalar array inline cap | 8 elements — a longer array of scalars breaks one per line |

Braces are padded when inline (`{ "var": "x" }`), brackets are not
(`[1, 2]`). Output ends with exactly one newline; a BOM is removed; line
endings are `\n`.

## What always breaks

A document, a task, a task group, a fragment call site (`use`), and every
`tasks` array print one key or one step per line, whatever their size. A
workflow is read top to bottom; nothing about its shape is inlined. The
`mappings` and `rules` arrays break one entry per line once they hold more
than one entry. An object keyed entirely by dotted paths — a case file's
`expect`, a use-case's assertions — is a checklist and breaks one entry per
line.

## What inlines when it fits

A function header (`"function": { "name": …, "input": { … } }`), a mapping,
a validation rule, a `loop` object, an array of scalars, and any object the
formatter does not recognise print on one line when the whole line fits in
the width — otherwise they break, one member per line, and the rule applies
again to each member.

## JSONLogic

An **operator node** is a single-key object whose key is an operator the
engine evaluates (`var`, `>=`, `and`, `cat`, `secret`, …). It is recognised
wherever it appears — a `condition`, a mapping's `logic`, a `filter`, a
query-dialect filter — and laid out by its **shape**:

| Shape | Definition | Layout |
|---|---|---|
| **Unary** | The argument is a scalar, a read (`var`, `val`, `secret`, `missing`), or a one-element array of either | **Always one line**, whatever the width: `{ "var": "data.x" }`, `{ "!": { "var": "data.ok" } }`, `{ "length": [{ "var": "data.items" }] }` |
| **Leaf** | Every argument is an atom — a scalar, a short array of scalars, a unary node, or a single-key object holding one (`{ "field": "id" }`, `{ "param": "customer_id" }`); or a unary operator wrapping a leaf | **One line when it fits**: `{ ">=": [{ "var": "data.order.amount" }, 500] }`, `{ "in": [{ "var": "data.tier" }, ["vip", "premium"]] }`, `{ "!": { "in": [{ "var": "data.id" }, [1, 7, 42]] } }` |
| **Compound** | Anything deeper | **Always breaks**, one argument per line; each argument is then laid out by its own shape |

So a condition reads:

```json
"condition": {
  "and": [
    { ">=": [{ "var": "data.order.amount" }, 100] },
    { "<": [{ "var": "data.order.amount" }, 500] }
  ]
}
```

The nesting of an expression is visible in its indentation, and a leaf
comparison — the thing most conditions are — is one line you can read.

## Canonical key order

Known keys of known shapes are written in the order below; keys not in a
table follow them, in the order you wrote them. `$from` is written first in
any object it appears in, because it is the base the rest of the object
overrides.

| Shape | Order |
|---|---|
| Workflow | `workflow_id`, `name`, `description`, `tags`, `priority`, `condition`, `loop`, `continue_on_error`, `activate`, `tasks` |
| Task | `id`, `name`, `description`, `condition`, `terminal`, `continue_on_error`, `function` |
| Task group | `id`, `name`, `description`, `condition`, `terminal`, `tasks` |
| Fragment call site | `id`, `use`, `with` |
| `function` | `name`, `input` |
| `function.input` | The field order of the function's table on the [Functions reference](./functions.md) |
| Mapping | `path`, `logic` |
| Validation rule | `logic`, `message` |
| `loop` | `counter`, `init`, `max`, `increment` |
| Channel | `channel_id`, `name`, `description`, `tags`, `channel_type`, `protocol`, `methods`, `route_pattern`, `topic`, `consumer_group`, `priority`, `workflow_id`, `activate`, `transport_config`, `config` |
| Connector | `id`, `name`, `connector_type`, `enabled`, `tags`, `config` |
| Shared document | `constants`, `errors`, `fragments` |
| Fragment | `params`, `tasks` |
| Case file | `name`, `workflow`, `input`, `metadata`, `secrets`, `stubs`, `stubs_file`, `expect`, `expect_errors`, `expect_calls`, `expect_tasks` |
| Package artifact | `package`, `requires`, `connectors`, `workflows`, `channels` |

A channel's `config`, a connector's `config` and every payload keep your
order.

## How a document is recognised

By shape, never by file name: an object with `tasks` is a workflow, with
`connector_type` a connector, with `channel_type` or `protocol` a channel,
with `constants`/`errors`/`fragments` a shared document, with `workflow` +
`input` + `expect` a case file, with `package` + `workflows` a promotion
artifact. A root array of entities (a bulk-import body) and the arrays
inside an artifact are recognised the same way, and a bare array of steps —
what an editor sends through `--stdin` for a selected `tasks` array — is
laid out as a task list.

## Strictness

`fmt` refuses, and leaves untouched, a file that is not strict JSON, a file
with a **duplicate key** (the runtime would silently keep the last, so the
file does not mean what it appears to), and nesting deeper than the runtime's
parser accepts. Each refusal names the file, line and column; the other
files in the run are still formatted.

## Related

- [CLI Reference — `fmt`](./cli.md#fmt) — the command, its flags and exit codes
- [Test Workflows Offline](../build/testing.md) — where `fmt --check` sits in a CI gate
