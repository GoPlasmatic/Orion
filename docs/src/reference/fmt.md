<!-- description: The one layout `orion-server fmt` writes: the numbers, the key order of every recognised shape, and the JSONLogic inlining rules — with nothing to configure. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-25 -->

# Definition style (`fmt`)

`orion-server fmt` rewrites definition files to one layout, the way `gofmt` and `cargo fmt` do. There is no configuration: every file in every tree reads the same way. A style change is a change to Orion rather than to a project's settings.

This page is that style, stated as rules. The command itself is under [`fmt`](./cli/orion-server/fmt.md), and [Test a workflow offline](../guides/author/testing.md) is where `fmt --check` sits in a CI gate.

## What the formatter changes, and what it never touches

It changes whitespace, string escapes (re-emitted canonically), and the order of known keys in known shapes (the tables in [Canonical key order](#canonical-key-order)).

It never changes a value or the spelling of a number: `1.0` stays `1.0`, `1e3` stays `1e3`. It never reorders array elements, or keys it does not recognize. A connector's `config`, an `http_call` body and a case file's `input` are written back in the order you wrote them. Before anything is written, the output is parsed again and compared with the input as the runtime sees it. A difference is reported as a formatter bug and the file is left alone.

## The numbers

| Setting | Value |
|---|---|
| Line width | 100 columns — a node prints on one line when it fits |
| Indent | 2 spaces |
| Scalar array inline cap | 8 elements — a longer array of scalars breaks one per line |

Braces are padded when inline (`{ "var": "x" }`), brackets are not (`[1, 2]`). Output ends with exactly one newline; a BOM is removed; line endings are `\n`.

## What always breaks

A document, a task, a task group, a fragment call site (`use`), and every `tasks` array print one key or one step per line. Their size does not matter. A workflow is read top to bottom; nothing about its shape is inlined. The `mappings` and `rules` arrays break one entry per line once they hold more than one entry. An object keyed entirely by dotted paths, such as a case file's `expect` or a use-case's assertions, is a checklist. It breaks one entry per line.

## What inlines when it fits

A function header (`"function": { "name": …, "input": { … } }`), a mapping, a validation rule, a `loop` object and an array of scalars are leaves. A leaf prints on one line when the whole line fits in the width. So does any object the formatter does not recognize. Otherwise they break, one member per line, and the rule applies again to each member.

## JSONLogic

An operator node is a single-key object whose key is an operator the engine evaluates (`var`, `>=`, `and`, `cat`, `secret`, …). It is recognized wherever it appears, in a `condition`, a mapping's `logic`, a `filter` or a query-dialect filter, and laid out by its shape:

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

The nesting of an expression is visible in its indentation. A leaf comparison, which is what most conditions are, is one line you can read.

## Canonical key order

Known keys of known shapes are written in the order in the table. Keys not in a table follow them, in the order you wrote them. `$each`, `$from` and `$use` are written first in any object they appear in. Each is the base the rest of the object overrides or repeats. A `$use`'s `with` follows it.

| Shape | Order |
|---|---|
| Workflow | `workflow_id`, `name`, `description`, `tags`, `priority`, `condition`, `loop`, `continue_on_error`, `activate`, `tasks` |
| Task | `id`, `name`, `description`, `condition`, `terminal`, `halt_on`, `continue_on_error`, `for_each`, `function` |
| Task group | `id`, `name`, `description`, `condition`, `terminal`, `tasks` |
| Fragment call site | `id`, `use`, `with` |
| `function` | `name`, `input` |
| `function.input` | The field order of the function's table on the [Functions reference](./functions/index.md) |
| Mapping | `path`, `logic` |
| Validation rule | `logic`, `message` |
| `loop` | `counter`, `init`, `max`, `increment`, `over`, `as`, `scratch`, `setup` |
| `for_each` | `over`, `as`, `max_concurrency`, `collect`, `into` |
| Channel | `channel_id`, `name`, `description`, `tags`, `channel_type`, `protocol`, `methods`, `route_pattern`, `topic`, `consumer_group`, `priority`, `workflow_id`, `activate`, `transport_config`, `config` |
| Connector | `id`, `name`, `connector_type`, `enabled`, `tags`, `config` |
| Shared document | `package`, `constants`, `errors`, `fragments` |
| Package declaration | `name`, `requires` |
| Fragment | `params`, `tasks`, `value` |
| `$each` element | `$each`, `do` |
| Case file | `name`, `workflow`, `input`, `metadata`, `secrets`, `stubs`, `stubs_file`, `expect`, `expect_errors`, `expect_calls`, `expect_tasks` |
| Package artifact | `package`, `requires`, `connectors`, `workflows`, `channels` |

A channel's `config`, a connector's `config` and every payload keep your
order.

## How a document is recognized

By shape, never by file name. An object with `tasks` is a workflow, with `connector_type` a connector, and with `channel_type` or `protocol` a channel. One with `constants`, `errors` or `fragments` is a shared document. So is one whose `package` object has no `content_hash`, which is the set's package declaration. One with `workflow` + `input` + `expect` is a case file, and one with `package` + `workflows` a promotion artifact. A root array of entities (a bulk-import body) and the arrays inside an artifact are recognized the same way. A bare array of steps, which is what an editor sends through `--stdin` for a selected `tasks` array, is laid out as a task list.

## Errors

`fmt` refuses, and leaves untouched, three kinds of file. One is not strict JSON. One holds a duplicate key, which the runtime would silently resolve by keeping the last, so the file does not mean what it appears to. One nests deeper than the runtime's parser accepts. Each refusal names the file, line and column; the other files in the run are still formatted.

## Related

- [CLI › `fmt`](./cli/orion-server/fmt.md): the command, its flags and exit codes.
- [Test a workflow offline](../guides/author/testing.md): where `fmt --check` sits in a CI gate.
- [Advisory checks (`clippy`)](./clippy/index.md): the rules that run after `lint`, which `fmt` never applies.
