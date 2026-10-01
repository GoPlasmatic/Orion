<!-- description: The map task function: an ordered list of JSONLogic mappings, each writing its result to a dotted path in the data context, for reshaping and enriching. -->
<!-- type: reference -->
<!-- last_verified: 2026-10-01 -->

# `map`

Applies an ordered list of JSONLogic expressions, writing each result to a
dotted path in the context. The primary tool for reshaping, computing, and
enriching data.

## Synopsis

```json
{
  "name": "map",
  "input": {
    "mappings": []
  }
}
```

## Description

`map` is an engine built-in from dataflow-rs. Orion declares no input schema for it. The engine checks each mapping's shape when the workflow is saved. A problem only a message can reveal surfaces when the task runs, such as appending to a value that is not an array.

**Retry safety:** `pure`. An engine built-in reads and writes the message and nothing else.

## Fields

| Field | Type | Required | Default | Description |
|-------|------|:--------:|---------|-------------|
| `mappings` | array | yes | — | Ordered list of mappings, applied in order |
| `mappings[].path` | string \| JSONLogic | yes | — | Dotted target path, for example `"data.order.total"`. An expression computes the path per message |
| `mappings[].logic` | JSONLogic | yes, unless `unset` | — | Expression whose result is written to `path` |
| `mappings[].mode` | string | no | `"set"` | How a result is written: `set` replaces the value at `path`; `append` pushes the result onto the array there; `extend` pushes each element of an array result. Needs `logic` |
| `mappings[].on_null` | string | no | `"skip"` | What a `null` result does: `skip` leaves `path` as it was; `unset` removes it. Needs `logic` |
| `mappings[].unset` | bool | no | `false` | Remove the key at `path` instead of writing it. Takes no `logic` |

A mapping whose result is `null` writes nothing by default: the path keeps the value it had. That is what lets `{"if": [cond, value, null]}` mean "set or keep". To clear a slot, use `"unset": true`, or `"on_null": "unset"` for "set or clear". [`correctness.mapping_always_null`](../clippy/correctness-mapping-always-null.md) reports a mapping that is always null and would write nothing.

`append` and `extend` add to an array in place. A missing or `null` target becomes a new array. Any other non-array target fails the mapping rather than being wrapped, and so does an `extend` whose result is not an array. Each append copies only the new entry. The `merge` idiom (`{"merge": [{"var": "data.log"}, [entry]]}`) rebuilt the whole array on every write, which made a log kept across a [loop](../workflows.md) quadratic.

A mapping that removes, appends or extends cannot target a context root (`data`, `metadata`, `temp_data`). Neither `mode` nor `on_null` can be given without `logic`. Both mistakes are refused when the workflow is saved.

## Examples

```json
{
  "name": "map",
  "input": {
    "mappings": [
      { "path": "data.order.flagged", "logic": true },
      { "path": "data.order.total_with_tax", "logic": { "*": [{ "var": "data.order.total" }, 1.1] } }
    ]
  }
}
```

Keep a log across the sweeps of a loop, one entry per sweep:

```json
{
  "name": "map",
  "input": {
    "mappings": [
      { "path": "data.log", "logic": { "var": "temp_data.entry" }, "mode": "append" }
    ]
  }
}
```

## Related

- [Workflows](../../concepts/workflows.md): the pipeline model these functions run in.
- [Author a workflow](../../guides/author/workflows.md): conditions, mapping and validation in practice.
- [Task functions](./index.md): the whole catalogue, and each function's retry safety.
- [Workflow definition](../workflows.md#the-data-context): the data context every function reads and writes.
