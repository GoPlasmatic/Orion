<!-- description: The `correctness.mapping_overwritten` advisory rule, level `warn`, scope workflow: two mappings in one map write the same path with nothing reading it in. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `correctness.mapping_overwritten`

A `warn` rule, scope workflow. Two mappings in one map write the same path with nothing reading it in between.

## Synopsis

```console
$ orion-server clippy ./definitions
warn[correctness.mapping_overwritten] two mappings in one map write the same path with nothing reading it in between
```

## Description

Two mappings of one `map` write the same `path`. Neither the mappings between them nor the second one reads it, or anything inside or above it. `data.x = data.x + 1` is a pattern and stays silent.

## Related

- [Advisory checks](./index.md): every rule, the levels, and where certainty comes from.
- [`orion-server clippy`](../cli/orion-server/clippy.md): the command, its flags and exit codes.
- [Test workflows offline](../../guides/author/testing.md): running the rules against a set.
- [Workflows](../workflows.md): the step grammar the rule reads.
