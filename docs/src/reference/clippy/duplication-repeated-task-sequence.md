<!-- description: The `duplication.repeated_task_sequence` advisory rule, level `warn`, scope set: the same run of two or more steps appears three or more times across the set. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# `duplication.repeated_task_sequence`

A `warn` rule, scope set. The same run of two or more steps appears three or more times across the set.

## Synopsis

```console
$ orion-server clippy ./definitions
warn[duplication.repeated_task_sequence] the same run of two or more steps appears three or more times across the set
```

## Description

A run of ≥ 2 steps, ids and names stripped, in ≥ 3 places. The finding is the longest such run at those places. The message states the fact and where. Whether it should be a fragment is the author's call. A `use`, `$use` or `$each` step breaks a run, as a step that is already shared.

## Related

- [Advisory checks](./index.md): every rule, the levels, and where certainty comes from.
- [`orion-server clippy`](../cli/orion-server/clippy.md): the command, its flags and exit codes.
- [Test workflows offline](../../guides/author/testing.md): running the rules against a set.
- [Shared definitions](../cli/shared-definitions.md): the set the rule reads, and its fragments.
