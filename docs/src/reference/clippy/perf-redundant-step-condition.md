<!-- description: The `perf.redundant_step_condition` advisory rule, level `warn`, scope workflow: consecutive steps repeat one condition that none of them can change; a task. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `perf.redundant_step_condition`

A `warn` rule, scope workflow. Consecutive steps repeat one condition that none of them can change; a task group evaluates it once.

## Synopsis

```console
$ orion-server clippy ./definitions
warn[perf.redundant_step_condition] consecutive steps repeat one condition that none of them can change; a task group evaluates it once
```

## Description

Consecutive steps in one list with a byte-identical condition that none of them can change. No step in the run writes a path the condition reads, and the condition has no computed, scoped or nondeterministic part. A task group would evaluate it once.

## Caveats

Suggestion only.

## Related

- [Advisory checks](./index.md): every rule, the levels, and where certainty comes from.
- [`orion-server clippy`](../cli/orion-server/clippy.md): the command, its flags and exit codes.
- [Test workflows offline](../../guides/author/testing.md): running the rules against a set.
- [Workflows](../workflows.md): the step grammar the rule reads.
