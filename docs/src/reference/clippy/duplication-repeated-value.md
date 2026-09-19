<!-- description: The `duplication.repeated_value` advisory rule, level `warn`, scope set: the same object literal appears three or more times across the set. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `duplication.repeated_value`

A `warn` rule, scope set. The same object literal appears three or more times across the set.

## Synopsis

```console
$ orion-server clippy ./definitions
warn[duplication.repeated_value] the same object literal appears three or more times across the set
```

## Description

An object with ≥ 2 keys, byte-identical in ≥ 3 places.

## Caveats

Silent for structure: steps, `function` headers, mapping and rule entries, operator nodes, roots. Silent for the input of an engine built-in, for a `use` step's `with` block, and for anything containing a `$from` or a `$sql`. Silent for an object whose every occurrence sits inside a larger reported one.

## Related

- [Advisory checks](./index.md): every rule, the levels, and where certainty comes from.
- [`orion-server clippy`](../cli/orion-server/clippy.md): the command, its flags and exit codes.
- [Test workflows offline](../../guides/author/testing.md): running the rules against a set.
- [Shared definitions](../cli/shared-definitions.md): the set the rule reads, and its fragments.
