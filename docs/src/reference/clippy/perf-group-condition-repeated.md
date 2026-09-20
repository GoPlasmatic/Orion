<!-- description: The `perf.group_condition_repeated` advisory rule, level `warn`, scope workflow: a group member repeats the group's own condition, which was already true on. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-19 -->

# `perf.group_condition_repeated`

A `warn` rule, scope workflow. A group member repeats the group's own condition, which was already true on entry.

## Synopsis

```console
$ orion-server clippy ./definitions
warn[perf.group_condition_repeated] a group member repeats the group's own condition, which was already true on entry
```

## Description

A group member repeating its group's condition, when no earlier member writes what it reads. It was already true on entry.

The rule is silent when the condition reads `metadata.progress`. The engine overwrites that path after every task, so an earlier member always changes it.

## Related

- [Advisory checks](./index.md): every rule, the levels, and where certainty comes from.
- [`orion-server clippy`](../cli/orion-server/clippy.md): the command, its flags and exit codes.
- [Test workflows offline](../../guides/author/testing.md): running the rules against a set.
- [Workflows](../workflows.md): the step grammar the rule reads.
