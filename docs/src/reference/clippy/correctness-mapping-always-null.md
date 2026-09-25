<!-- description: The `correctness.mapping_always_null` advisory rule, level `deny`, scope workflow: a `map` mapping whose `logic` is always null, so it never writes. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-25 -->

# `correctness.mapping_always_null`

A `deny` rule, scope workflow. A `map` mapping whose `logic` is always null, so it never writes.

## Synopsis

```console
$ orion-server clippy ./definitions
deny[correctness.mapping_always_null] a `map` mapping whose `logic` is always null, so it never writes
```

## Description

A `map` mapping whose `logic` the datalogic compiler folds to the constant `null`: `"logic": null`, or an expression such as `{"if": [false, 1, null]}`. `map` skips the assignment when a mapping's result is `null`, so the path keeps whatever it held. Used to clear a slot, the mapping silently does nothing. Inside a `loop` the slot still holds the previous iteration's value, which is how a retry flag survives into the next item.

To remove the path, write the mapping as `"unset": true`, which dataflow-rs 3.14 added for exactly this. Otherwise remove the mapping.

## Caveats

Silent when the `logic` is not a compile-time constant. `{"var": "temp_data.maybe"}` may be null only at run time. `{"if": [c, x, null]}` uses `null` on one branch to mean "keep the current value", which is a legitimate reading. Silent too for a mapping with `"unset": true`, and for one whose `on_null` is anything but `skip`: `on_null: "unset"` removes the path when the result is null, so an always-null `logic` there writes on every message. See [`map`](../functions/map.md) for the null rule itself.

## Related

- [Advisory checks](./index.md): every rule, the levels, and where certainty comes from.
- [`orion-server clippy`](../cli/orion-server/clippy.md): the command, its flags and exit codes.
- [Test workflows offline](../../guides/author/testing.md): running the rules against a set.
- [Workflows](../workflows.md): the step grammar the rule reads.
